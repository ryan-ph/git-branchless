//! Wrapper around the Eden SCM directed acyclic graph implementation, which
//! allows for efficient graph queries.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::iter::FromIterator;

use eden_dag::ops::DagPersistent;
use eden_dag::DagAlgorithm;
use eyre::Context;
use itertools::Itertools;
use once_cell::sync::OnceCell;
use tracing::{instrument, trace, warn};

use crate::core::effects::{Effects, OperationType};
use crate::core::eventlog::{CommitActivityStatus, EventCursor, EventReplayer};
use crate::git::{Commit, MaybeZeroOid, NonZeroOid, Repo, Time};

use super::repo_ext::RepoReferencesSnapshot;

impl From<NonZeroOid> for eden_dag::VertexName {
    fn from(oid: NonZeroOid) -> Self {
        eden_dag::VertexName::copy_from(oid.as_bytes())
    }
}

impl TryFrom<eden_dag::VertexName> for MaybeZeroOid {
    type Error = eyre::Error;

    fn try_from(value: eden_dag::VertexName) -> Result<Self, Self::Error> {
        let oid = git2::Oid::from_bytes(value.as_ref())?;
        let oid = MaybeZeroOid::from(oid);
        Ok(oid)
    }
}

impl TryFrom<eden_dag::VertexName> for NonZeroOid {
    type Error = eyre::Error;

    fn try_from(value: eden_dag::VertexName) -> Result<Self, Self::Error> {
        let oid = MaybeZeroOid::try_from(value)?;
        let oid = NonZeroOid::try_from(oid)?;
        Ok(oid)
    }
}

/// A compact set of commits, backed by the Eden DAG.
pub type CommitSet = eden_dag::NameSet;

/// A vertex referring to a single commit in the Eden DAG.
pub type CommitVertex = eden_dag::VertexName;

impl From<NonZeroOid> for CommitSet {
    fn from(oid: NonZeroOid) -> Self {
        let vertex = CommitVertex::from(oid);
        CommitSet::from_static_names([vertex])
    }
}

impl FromIterator<NonZeroOid> for CommitSet {
    fn from_iter<T: IntoIterator<Item = NonZeroOid>>(iter: T) -> Self {
        let oids = iter
            .into_iter()
            .map(CommitVertex::from)
            .map(Ok)
            .collect_vec();
        CommitSet::from_iter(oids)
    }
}

/// Eagerly convert a `CommitSet` into a `Vec<NonZeroOid>` by iterating over it, preserving order.
#[instrument]
pub fn commit_set_to_vec(commit_set: &CommitSet) -> eyre::Result<Vec<NonZeroOid>> {
    let mut result = Vec::new();
    for vertex in commit_set.iter().wrap_err("Iterating commit set")? {
        let vertex = vertex.wrap_err("Evaluating vertex")?;
        let vertex = NonZeroOid::try_from(vertex.clone())
            .wrap_err_with(|| format!("Converting vertex to NonZeroOid: {:?}", &vertex))?;
        result.push(vertex);
    }
    Ok(result)
}

/// Union together a list of [CommitSet]s.
pub fn union_all(commits: &[CommitSet]) -> CommitSet {
    commits
        .iter()
        .fold(CommitSet::empty(), |acc, elem| acc.union(elem))
}

/// Interface to access the directed acyclic graph (DAG) representing Git's
/// commit graph. Based on the Eden SCM DAG.
pub struct Dag {
    inner: eden_dag::Dag,

    /// A set containing the commit which `HEAD` points to. If `HEAD` is unborn,
    /// this is an empty set.
    pub head_commit: CommitSet,

    /// A set containing the commit that the main branch currently points to.
    pub main_branch_commit: CommitSet,

    /// A set containing all commits currently pointed to by local branches.
    pub branch_commits: CommitSet,

    /// A set containing all commits that have been observed by the
    /// `EventReplayer`.
    observed_commits: CommitSet,

    /// A set containing all commits that have been determined to be obsolete by
    /// the `EventReplayer`.
    obsolete_commits: CommitSet,

    public_commits: OnceCell<CommitSet>,
    visible_heads: OnceCell<CommitSet>,
    visible_commits: OnceCell<CommitSet>,
    draft_commits: OnceCell<CommitSet>,
}

impl Dag {
    /// Initialize the DAG for the given repository, and update it with any
    /// newly-referenced commits.
    #[instrument]
    pub fn open_and_sync(
        effects: &Effects,
        repo: &Repo,
        event_replayer: &EventReplayer,
        event_cursor: EventCursor,
        references_snapshot: &RepoReferencesSnapshot,
    ) -> eyre::Result<Self> {
        let mut dag = Self::open_without_syncing(
            effects,
            repo,
            event_replayer,
            event_cursor,
            references_snapshot,
        )?;
        dag.sync(effects, repo)?;
        Ok(dag)
    }

    /// Initialize a DAG for the given repository, without updating it with new
    /// commits that may have appeared.
    ///
    /// If used improperly, commit lookups could fail at runtime. This function
    /// should only be used for opening the DAG when it's known that no more
    /// live commits have appeared.
    #[instrument]
    pub fn open_without_syncing(
        effects: &Effects,
        repo: &Repo,
        event_replayer: &EventReplayer,
        event_cursor: EventCursor,
        references_snapshot: &RepoReferencesSnapshot,
    ) -> eyre::Result<Self> {
        let observed_commits = event_replayer.get_cursor_oids(event_cursor);
        let RepoReferencesSnapshot {
            head_oid,
            main_branch_oid,
            branch_oid_to_names,
        } = references_snapshot;

        let obsolete_commits: CommitSet = observed_commits
            .iter()
            .copied()
            .filter(|commit_oid| {
                match event_replayer.get_cursor_commit_activity_status(event_cursor, *commit_oid) {
                    CommitActivityStatus::Active | CommitActivityStatus::Inactive => false,
                    CommitActivityStatus::Obsolete => true,
                }
            })
            .collect();

        let dag_dir = repo.get_dag_dir();
        std::fs::create_dir_all(&dag_dir).wrap_err("Creating .git/branchless/dag dir")?;
        let dag = eden_dag::Dag::open(&dag_dir)
            .wrap_err_with(|| format!("Opening DAG directory at: {:?}", &dag_dir))?;

        let observed_commits: CommitSet = observed_commits.into_iter().collect();
        let head_commit = match head_oid {
            Some(head_oid) => CommitSet::from(*head_oid),
            None => CommitSet::empty(),
        };
        let main_branch_commit = CommitSet::from(*main_branch_oid);
        let branch_commits: CommitSet = branch_oid_to_names.keys().copied().collect();

        Ok(Self {
            inner: dag,
            head_commit,
            main_branch_commit,
            branch_commits,
            observed_commits,
            obsolete_commits,
            public_commits: Default::default(),
            visible_heads: Default::default(),
            visible_commits: Default::default(),
            draft_commits: Default::default(),
        })
    }

    /// This function's code adapted from `GitDag`, licensed under GPL-2.
    #[instrument]
    fn sync(&mut self, effects: &Effects, repo: &Repo) -> eyre::Result<()> {
        let master_heads = self.main_branch_commit.clone();
        let non_master_heads = self
            .observed_commits
            .union(&self.head_commit)
            .union(&self.branch_commits);
        self.sync_from_oids(effects, repo, master_heads, non_master_heads)
    }

    /// Update the DAG with the given heads.
    #[instrument]
    pub fn sync_from_oids(
        &mut self,
        effects: &Effects,
        repo: &Repo,
        master_heads: CommitSet,
        non_master_heads: CommitSet,
    ) -> eyre::Result<()> {
        let (effects, _progress) = effects.start_operation(OperationType::UpdateCommitGraph);
        let _effects = effects;

        let parent_func = |v: CommitVertex| -> eden_dag::Result<Vec<CommitVertex>> {
            use eden_dag::errors::BackendError;
            trace!(?v, "visiting Git commit");

            let oid = MaybeZeroOid::from_bytes(v.as_ref())
                .map_err(|_e| anyhow::anyhow!("Could not convert to Git oid: {:?}", &v))
                .map_err(BackendError::Other)?;
            let oid = match oid {
                MaybeZeroOid::NonZero(oid) => oid,
                MaybeZeroOid::Zero => return Ok(Vec::new()),
            };

            let commit = repo
                .find_commit(oid)
                .map_err(|_e| anyhow::anyhow!("Could not resolve to Git commit: {:?}", &v))
                .map_err(BackendError::Other)?;
            let commit = match commit {
                Some(commit) => commit,
                None => {
                    // This might be an OID that's been garbage collected, or
                    // just a non-commit object. Ignore it in either case.
                    return Ok(Vec::new());
                }
            };

            Ok(commit
                .get_parent_oids()
                .into_iter()
                .map(CommitVertex::from)
                .collect())
        };

        let commit_set_to_vec = |commit_set: CommitSet| -> Vec<CommitVertex> {
            let mut result = Vec::new();
            for vertex in commit_set
                .iter()
                .expect("The commit set was produced statically, so iteration should not fail")
            {
                let vertex = vertex.expect(
                    "The commit set was produced statically, so accessing a vertex should not fail",
                );
                result.push(vertex);
            }
            result
        };
        self.inner.add_heads_and_flush(
            parent_func,
            commit_set_to_vec(master_heads).as_slice(),
            commit_set_to_vec(non_master_heads).as_slice(),
        )?;
        Ok(())
    }

    /// Create a new version of this DAG at the point in time represented by
    /// `event_cursor`.
    pub fn set_cursor(
        &self,
        effects: &Effects,
        repo: &Repo,
        event_replayer: &EventReplayer,
        event_cursor: EventCursor,
    ) -> eyre::Result<Self> {
        let references_snapshot = event_replayer.get_references_snapshot(repo, event_cursor)?;
        let dag = Self::open_without_syncing(
            effects,
            repo,
            event_replayer,
            event_cursor,
            &references_snapshot,
        )?;
        Ok(dag)
    }

    /// Get the parent OID for the given OID. Returns an error if the given OID
    /// does not have exactly 1 parent.
    #[instrument]
    pub fn get_only_parent_oid(&self, oid: NonZeroOid) -> eyre::Result<NonZeroOid> {
        let parents: CommitSet = self.inner.parents(CommitSet::from(oid))?;
        match commit_set_to_vec(&parents)?[..] {
            [oid] => Ok(oid),
            [] => Err(eyre::eyre!("Commit {} has no parents.", oid)),
            _ => Err(eyre::eyre!("Commit {} has more than 1 parents.", oid)),
        }
    }

    /// Get the range of OIDs from `parent_oid` to `child_oid`. Note that there
    /// may be more than one path; in that case, the OIDs are returned in a
    /// topologically-sorted order.
    #[instrument]
    pub fn get_range(
        &self,
        effects: &Effects,
        repo: &Repo,
        parent_oid: NonZeroOid,
        child_oid: NonZeroOid,
    ) -> eyre::Result<Vec<NonZeroOid>> {
        let (effects, _progress) = effects.start_operation(OperationType::WalkCommits);
        let _effects = effects;

        let roots = CommitSet::from_static_names(vec![CommitVertex::from(parent_oid)]);
        let heads = CommitSet::from_static_names(vec![CommitVertex::from(child_oid)]);
        let range = self.inner.range(roots, heads).wrap_err("Computing range")?;
        let range = self.inner.sort(&range).wrap_err("Sorting range")?;
        let oids = {
            let mut result = Vec::new();
            for vertex in range.iter()? {
                let vertex = vertex?;
                let oid = vertex.as_ref();
                let oid = MaybeZeroOid::from_bytes(oid)?;
                match oid {
                    MaybeZeroOid::Zero => {
                        // Do nothing.
                    }
                    MaybeZeroOid::NonZero(oid) => result.push(oid),
                }
            }
            result
        };
        Ok(oids)
    }

    /// Conduct an arbitrary query against the DAG.
    pub fn query(&self) -> &eden_dag::Dag {
        self.inner.borrow()
    }

    /// Determine whether or not the given commit is a public commit (i.e. is an
    /// ancestor of the main branch).
    #[instrument]
    pub fn is_public_commit(&self, commit_oid: NonZeroOid) -> eyre::Result<bool> {
        let main_branch_commits = commit_set_to_vec(&self.main_branch_commit)?;
        for main_branch_commit in main_branch_commits {
            if self
                .inner
                .is_ancestor(commit_oid.into(), main_branch_commit.into())?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Return the set of commits which are public, as per the definition in
    /// `is_public_commit`. You should try to use `is_public_commit` instead, as
    /// it will be faster to compute.
    #[instrument]
    pub fn query_public_commits_slow(&self) -> eyre::Result<&CommitSet> {
        self.public_commits.get_or_try_init(|| {
            let public_commits = self.query().ancestors(self.main_branch_commit.clone())?;
            Ok(public_commits)
        })
    }

    /// Determine the set of commits which are considered to be "visible". A
    /// commit is "visible" if it is not obsolete or has a non-obsolete
    /// descendant.
    #[instrument]
    pub fn query_visible_heads(&self) -> eyre::Result<&CommitSet> {
        self.visible_heads.get_or_try_init(|| {
            let visible_heads = CommitSet::empty()
                .union(&self.observed_commits.difference(&self.obsolete_commits))
                .union(&self.head_commit)
                .union(&self.main_branch_commit)
                .union(&self.branch_commits);
            let visible_heads = self.query().heads(visible_heads)?;
            Ok(visible_heads)
        })
    }

    /// Query the set of all visible commits, as per the definition in
    /// `query_visible_head`s. You should try to use `query_visible_heads`
    /// instead if possible, since it will be faster to compute.
    #[instrument]
    pub fn query_visible_commits_slow(&self) -> eyre::Result<&CommitSet> {
        self.visible_commits.get_or_try_init(|| {
            let visible_heads = self.query_visible_heads()?;
            let result = self.inner.ancestors(visible_heads.clone())?;
            Ok(result)
        })
    }

    /// Keep only commits in the given set which are visible, as per the
    /// definition in `query_visible_heads`.
    #[instrument]
    pub fn filter_visible_commits(&self, commits: CommitSet) -> eyre::Result<CommitSet> {
        let visible_heads = self.query_visible_heads()?;
        Ok(commits.intersection(&self.query().range(commits.clone(), visible_heads.clone())?))
    }

    /// Determine the set of obsolete commits. These commits have been rewritten
    /// or explicitly hidden by the user.
    #[instrument]
    pub fn query_obsolete_commits(&self) -> CommitSet {
        self.obsolete_commits.clone()
    }

    /// Determine the set of "draft" commits. The draft commits are all visible
    /// commits which aren't public.
    #[instrument]
    pub fn query_draft_commits(&self) -> eyre::Result<&CommitSet> {
        self.draft_commits.get_or_try_init(|| {
            let visible_heads = self.query_visible_heads()?;
            let draft_commits = self
                .query()
                .only(visible_heads.clone(), self.main_branch_commit.clone())?;
            Ok(draft_commits)
        })
    }

    /// Given a CommitSet, return a list of CommitSets, each representing a
    /// connected component of the set.
    ///
    /// For example, if the DAG contains commits A-B-C-D-E-F and the given
    /// CommitSet contains `B, C, E`, this will return 2 `CommitSet`s: 1
    /// containing `B, C` and another containing only `E`
    #[instrument]
    pub fn get_connected_components(&self, commit_set: &CommitSet) -> eyre::Result<Vec<CommitSet>> {
        let mut components: Vec<CommitSet> = Vec::new();
        let mut component = CommitSet::empty();
        let mut commits_to_connect = commit_set.clone();

        // FIXME: O(n^2) algorithm (
        // FMI see https://github.com/arxanas/git-branchless/pull/450#issuecomment-1188391763
        for commit in commit_set_to_vec(commit_set)? {
            if commits_to_connect.is_empty()? {
                break;
            }

            if !commits_to_connect.contains(&commit.into())? {
                continue;
            }

            let mut commits = CommitSet::from(commit);
            while !commits.is_empty()? {
                component = component.union(&commits);
                commits_to_connect = commits_to_connect.difference(&commits);

                let parents = self.query().parents(commits.clone())?;
                let children = self.query().children(commits.clone())?;
                commits = parents.union(&children).intersection(&commits_to_connect);
            }

            components.push(component);
            component = CommitSet::empty();
        }

        let connected_commits = union_all(&components);
        assert_eq!(commit_set.count()?, connected_commits.count()?);
        let connected_commits = commit_set.intersection(&connected_commits);
        assert_eq!(commit_set.count()?, connected_commits.count()?);

        Ok(components)
    }
}

impl std::fmt::Debug for Dag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<Dag>")
    }
}

/// Sort the given set of commits topologically. In the case of two commits
/// being unorderable, sort them using a deterministic tie-breaking function.
/// Commits which have been garbage collected and are no longer available in the
/// repository are omitted.
///
/// FIXME: this function does not use a total ordering for the sort, which could
/// mean that it produces incorrect results. Suppose that we have a graph with
/// parentage relationships A < B, B < C, A < D. Since D is not directly
/// comparable with B or C, it's possible that we calculate D < B and D > C,
/// which violates transitivity (D < B and B < C implies that D < C).
///
/// We only use this function to produce deterministic output, so in practice,
/// it doesn't seem to have a serious impact.
pub fn sorted_commit_set<'repo>(
    repo: &'repo Repo,
    dag: &Dag,
    commit_set: &CommitSet,
) -> eyre::Result<Vec<Commit<'repo>>> {
    let commit_oids = commit_set_to_vec(commit_set)?;
    let mut commits: Vec<Commit> = {
        let mut commits = Vec::new();
        for commit_oid in commit_oids {
            if let Some(commit) = repo.find_commit(commit_oid)? {
                commits.push(commit)
            }
        }
        commits
    };

    let commit_times: HashMap<NonZeroOid, Time> = commits
        .iter()
        .map(|commit| (commit.get_oid(), commit.get_time()))
        .collect();

    commits.sort_by(|lhs, rhs| {
        let lhs_vertex = CommitVertex::from(lhs.get_oid());
        let rhs_vertex = CommitVertex::from(rhs.get_oid());
        if dag
            .query()
            .is_ancestor(lhs_vertex.clone(), rhs_vertex.clone())
            .unwrap_or_else(|_| {
                warn!(
                    ?lhs_vertex,
                    ?rhs_vertex,
                    "Could not calculate `is_ancestor`"
                );
                false
            })
        {
            return Ordering::Less;
        } else if dag
            .query()
            .is_ancestor(rhs_vertex.clone(), lhs_vertex.clone())
            .unwrap_or_else(|_| {
                warn!(
                    ?lhs_vertex,
                    ?rhs_vertex,
                    "Could not calculate `is_ancestor`"
                );
                false
            })
        {
            return Ordering::Greater;
        }

        (&commit_times[&lhs.get_oid()], lhs.get_oid())
            .cmp(&(&commit_times[&rhs.get_oid()], rhs.get_oid()))
    });

    Ok(commits)
}
