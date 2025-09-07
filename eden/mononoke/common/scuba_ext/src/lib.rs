/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::hash_map::Entry;
use std::io::Error as IoError;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clientinfo::ClientRequestInfo;
use fbinit::FacebookInit;
use futures_stats::FutureStats;
use futures_stats::StreamStats;
use futures_stats::TryStreamStats;
use memory::MemoryStats;
use metadata::Metadata;
use nonzero_ext::nonzero;
use observability::ObservabilityContext;
use observability::ScubaLoggingDecisionFields;
pub use observability::ScubaVerbosityLevel;
use permission_checker::MononokeIdentitySetExt;
pub use sampling::Sampling;
pub use scribe_ext::ScribeClientImplementation;
use scuba::ScubaSample;
use scuba::ScubaSampleBuilder;
pub use scuba::ScubaValue;
use scuba::builder::ServerData;
use time_ext::DurationExt;

const FILE_PREFIX: &str = "file://";
const MAX_SCUBA_MSG_LEN: usize = 512000;

/// An extensible wrapper struct around `ScubaSampleBuilder`
#[derive(Clone)]
pub struct MononokeScubaSampleBuilder {
    inner: ScubaSampleBuilder,
    maybe_observability_context: Option<ObservabilityContext>,
    // This field decides if sampled out requests should
    // still be logged when verbose logging is enabled
    fallback_sampled_out_to_verbose: bool,
}

impl std::fmt::Debug for MononokeScubaSampleBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MononokeScubaSampleBuilder({:?})", self.inner)
    }
}

enum ScubaLoggingType<'a> {
    ScubaTable(&'a str),
    LocalFile(&'a str),
}

fn get_scuba_logging_type(arg: &str) -> ScubaLoggingType<'_> {
    if let Some(path) = arg.strip_prefix(FILE_PREFIX) {
        ScubaLoggingType::LocalFile(path)
    } else {
        ScubaLoggingType::ScubaTable(arg)
    }
}

impl MononokeScubaSampleBuilder {
    pub fn new(fb: FacebookInit, scuba_table: &str) -> Result<Self> {
        Ok(Self {
            inner: Self::get_scuba_sample_builder(fb, get_scuba_logging_type(scuba_table))?,
            maybe_observability_context: None,
            fallback_sampled_out_to_verbose: false,
        })
    }

    pub fn with_discard() -> Self {
        Self {
            inner: ScubaSampleBuilder::with_discard(),
            maybe_observability_context: None,
            fallback_sampled_out_to_verbose: false,
        }
    }

    pub fn with_opt_table(fb: FacebookInit, scuba_table: Option<String>) -> Result<Self> {
        match scuba_table {
            None => Ok(Self::with_discard()),
            Some(scuba_table) => Self::new(fb, &scuba_table),
        }
    }

    pub fn with_observability_context(self, octx: ObservabilityContext) -> Self {
        Self {
            maybe_observability_context: Some(octx),
            ..self
        }
    }

    fn get_scuba_sample_builder(
        fb: FacebookInit,
        scuba_logging_type: ScubaLoggingType,
    ) -> Result<ScubaSampleBuilder, IoError> {
        Ok(match scuba_logging_type {
            ScubaLoggingType::ScubaTable(scuba_table) => ScubaSampleBuilder::new(fb, scuba_table),
            ScubaLoggingType::LocalFile(path) => {
                ScubaSampleBuilder::with_discard().with_log_file(path)?
            }
        })
    }

    fn get_logging_decision_fields(&self) -> ScubaLoggingDecisionFields<'_> {
        ScubaLoggingDecisionFields {
            maybe_session_id: self.get("session_uuid"),
            maybe_unix_username: self.get("unix_username"),
            maybe_source_hostname: self.get("source_hostname"),
        }
    }

    pub fn should_log_with_level(&self, level: ScubaVerbosityLevel) -> bool {
        match level {
            ScubaVerbosityLevel::Normal => true,
            ScubaVerbosityLevel::Verbose => {
                self.maybe_observability_context
                    .as_ref()
                    .is_some_and(|octx| {
                        octx.should_log_scuba_sample(level, self.get_logging_decision_fields())
                    })
            }
        }
    }

    pub fn add<K: Into<String>, V: Into<ScubaValue>>(&mut self, key: K, value: V) -> &mut Self {
        self.inner.add(key, value);
        self
    }

    pub fn add_client_request_info(&mut self, client_info: &ClientRequestInfo) -> &mut Self {
        self.inner
            .add_opt("client_main_id", client_info.main_id.as_deref());
        self.inner
            .add("client_entry_point", client_info.entry_point.to_string());
        self.inner
            .add("client_correlator", client_info.correlator.as_str());

        // For context, see D76895703 or https://fburl.com/workplace/et4ezqp3.
        // Check the JK that disables reads for all blobstore ids being used
        // and log the ones that were disabled in this request.
        let disabled_reads_blobstore_ids = (1..4)
            .map(|id| id.to_string())
            .filter(|id| {
                justknobs::eval(
                    "scm/mononoke:disable_blobstore_reads",
                    Some(client_info.correlator.as_str()),
                    Some(id),
                )
                .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        if !disabled_reads_blobstore_ids.is_empty() {
            self.inner
                .add("disabled_reads_blobstore_ids", disabled_reads_blobstore_ids);
        }

        let read_bookmarks_from_xdb_replica = justknobs::eval(
            "scm/mononoke:read_bookmarks_from_xdb_replica",
            Some(client_info.correlator.as_str()),
            None,
        )
        .unwrap_or(false);

        self.inner.add(
            "read_bookmarks_from_xdb_replica",
            read_bookmarks_from_xdb_replica,
        );

        let use_maybe_stale_freshness_for_bookmarks =
            ["mononoke_api::repo::git::get_bookmark_state"]
                .into_iter()
                .map(|id| id.to_string())
                .filter(|id| {
                    justknobs::eval(
                        "scm/mononoke:use_maybe_stale_freshness_for_bookmarks",
                        Some(client_info.correlator.as_str()),
                        Some(id),
                    )
                    .unwrap_or(false)
                })
                .collect::<Vec<_>>();

        if !use_maybe_stale_freshness_for_bookmarks.is_empty() {
            self.inner.add(
                "use_maybe_stale_freshness_for_bookmarks",
                use_maybe_stale_freshness_for_bookmarks,
            );
        }
        self
    }

    pub fn add_metadata(&mut self, metadata: &Metadata) -> &mut Self {
        self.inner
            .add("session_uuid", metadata.session_id().to_string());

        self.inner.add(
            "client_identities",
            metadata
                .identities()
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>(),
        );

        if let Some(client_hostname) = metadata.client_hostname() {
            // "source_hostname" to remain compatible with historical logging
            self.inner
                .add("source_hostname", client_hostname.to_owned());
        } else if let Some(client_ip) = metadata.client_ip() {
            self.inner.add("client_ip", client_ip.to_string());
        }
        if let Some(unix_name) = metadata.unix_name() {
            // "unix_username" to remain compatible with historical logging
            self.inner.add("unix_username", unix_name);
        }

        if let Some(client_info) = metadata.client_request_info() {
            self.add_client_request_info(client_info);
        }

        self.inner
            .add_opt("sandcastle_alias", metadata.sandcastle_alias());
        self.inner
            .add_opt("sandcastle_vcs", metadata.sandcastle_vcs());
        self.inner
            .add_opt("revproxy_region", metadata.revproxy_region().as_deref());
        self.inner
            .add_opt("sandcastle_nonce", metadata.sandcastle_nonce());
        self.inner
            .add_opt("client_tw_job", metadata.clientinfo_tw_job());
        self.inner
            .add_opt("client_tw_task", metadata.clientinfo_tw_task());

        self.inner.add_opt("fetch_cause", metadata.fetch_cause());
        self.inner.add(
            "fetch_from_cas_attempted",
            metadata.fetch_from_cas_attempted(),
        );

        self
    }

    pub fn add_fetch_cause(&mut self, fetch_cause: &str) -> &mut Self {
        self.inner.add("fetch_cause", fetch_cause);
        self
    }

    pub fn add_fetch_from_cas_attempted(&mut self, fetch_from_cas_attempted: bool) -> &mut Self {
        self.inner
            .add("fetch_from_cas_attempted", fetch_from_cas_attempted);
        self
    }

    pub fn sample_for_identities(&mut self, identities: &impl MononokeIdentitySetExt) {
        // Details of quicksand traffic aren't particularly interesting because all Quicksand tasks are
        // doing effectively the same thing at the same time. If we need real-time debugging, we can
        // always rely on updating the verbosity in real time.
        if identities.is_quicksand() {
            self.sampled_unless_verbose(nonzero!(100u64));
        }
    }

    pub fn log_with_msg<S: Into<Option<String>>>(&mut self, log_tag: &str, msg: S) {
        if self.fallback_sampled_out_to_verbose
            && self.should_log_with_level(ScubaVerbosityLevel::Verbose)
        {
            // We need to unsample before we log, so that
            // `sample_rate` field is not added, as we are about
            // to log everything.
            self.inner.unsampled();
        }

        self.inner.add("log_tag", log_tag);
        if let Some(mut msg) = msg.into() {
            if MAX_SCUBA_MSG_LEN > 0 && msg.len() > MAX_SCUBA_MSG_LEN {
                msg.truncate(MAX_SCUBA_MSG_LEN);
                msg.push_str(" (...)");
            }

            self.inner.add("msg", msg);
        }
        self.inner.log();
    }

    /// Same as `log_with_msg`, but sample is assumed to be verbose and is only logged
    /// if verbose logging conditions are met
    pub fn log_with_msg_verbose<S: Into<Option<String>>>(&mut self, log_tag: &str, msg: S) {
        if !self.should_log_with_level(ScubaVerbosityLevel::Verbose) {
            return;
        }

        self.log_with_msg(log_tag, msg)
    }

    pub fn add_stream_stats(&mut self, stats: &StreamStats) -> &mut Self {
        self.inner
            .add("poll_count", stats.poll_count)
            .add("poll_time_us", stats.poll_time.as_micros_unchecked())
            .add(
                "max_poll_time_us",
                stats.max_poll_time.as_micros_unchecked(),
            )
            .add_opt(
                "completion_time_us",
                stats
                    .completion_time
                    .as_ref()
                    .map(Duration::as_micros_unchecked),
            )
            .add("count", stats.count);

        self
    }

    pub fn add_prefixed_stream_stats(&mut self, stats: &StreamStats) -> &mut Self {
        self.inner
            .add("stream_poll_count", stats.poll_count)
            .add("stream_poll_time_us", stats.poll_time.as_micros_unchecked())
            .add(
                "stream_max_poll_time_us",
                stats.max_poll_time.as_micros_unchecked(),
            )
            .add("stream_count", stats.count)
            .add("stream_completed", stats.completed as u32)
            .add_opt(
                "stream_completion_time_us",
                stats
                    .completion_time
                    .as_ref()
                    .map(Duration::as_micros_unchecked),
            )
            .add_opt(
                "stream_first_item_time_us",
                stats
                    .first_item_time
                    .as_ref()
                    .map(Duration::as_micros_unchecked),
            );

        self
    }

    pub fn add_future_stats(&mut self, stats: &FutureStats) -> &mut Self {
        self.inner
            .add("poll_count", stats.poll_count)
            .add("poll_time_us", stats.poll_time.as_micros_unchecked())
            .add(
                "max_poll_time_us",
                stats.max_poll_time.as_micros_unchecked(),
            )
            .add(
                "completion_time_us",
                stats.completion_time.as_micros_unchecked(),
            );

        self
    }

    pub fn add_try_stream_stats(&mut self, stats: &TryStreamStats) -> &mut Self {
        self.inner
            .add("poll_count", stats.stream_stats.poll_count)
            .add(
                "poll_time_us",
                stats.stream_stats.poll_time.as_micros_unchecked(),
            )
            .add(
                "max_poll_time_us",
                stats.stream_stats.max_poll_time.as_micros_unchecked(),
            )
            .add(
                "completion_time_us",
                stats
                    .stream_stats
                    .completion_time
                    .unwrap_or(Duration::ZERO)
                    .as_micros_unchecked(),
            )
            .add(
                "first_item_time_us",
                stats
                    .stream_stats
                    .completion_time
                    .unwrap_or(Duration::ZERO)
                    .as_micros_unchecked(),
            )
            .add("stream_chunk_count", stats.stream_stats.count)
            .add("stream_error_count", stats.error_count)
            .add("first_error_position", stats.first_error_position)
            .add("stream_completed", stats.stream_stats.completed as u32);

        self
    }

    pub fn add_memory_stats(&mut self, stats: &MemoryStats) -> &mut Self {
        self.inner
            .add("total_rss_bytes", stats.total_rss_bytes)
            .add("rss_free_bytes", stats.rss_free_bytes)
            .add("rss_free_pct", stats.rss_free_pct);

        self
    }

    pub fn is_discard(&self) -> bool {
        self.inner.is_discard()
    }

    pub fn sampled(&mut self, sample_rate: NonZeroU64) -> &mut Self {
        self.fallback_sampled_out_to_verbose = false;
        self.inner.sampled(sample_rate);
        self
    }

    pub fn sampled_unless_verbose(&mut self, sample_rate: NonZeroU64) -> &mut Self {
        self.fallback_sampled_out_to_verbose = true;
        self.inner.sampled(sample_rate);
        self
    }

    pub fn unsampled(&mut self) -> &mut Self {
        self.inner.unsampled();
        self
    }

    pub fn log(&mut self) -> bool {
        self.inner.log()
    }

    /// Same as `log`, but sample is assumed to be verbose and is only logged
    /// if verbose logging conditions are met
    pub fn log_verbose(&mut self) -> bool {
        if !self.should_log_with_level(ScubaVerbosityLevel::Verbose) {
            // Return value of the `log` function indicates whether
            // the sample passed sampling. If it's too verbose, let's
            // return false
            return false;
        }

        self.log()
    }

    pub fn add_common_server_data(&mut self) -> &mut Self {
        self.inner.add_common_server_data();
        self
    }

    pub fn sampling(&self) -> &Sampling {
        self.inner.sampling()
    }

    pub fn add_mapped_common_server_data<F>(&mut self, mapper: F) -> &mut Self
    where
        F: Fn(ServerData) -> &'static str,
    {
        self.inner.add_mapped_common_server_data(mapper);
        self
    }

    pub fn with_log_file<L: AsRef<Path>>(mut self, log_file: L) -> Result<Self, IoError> {
        self.inner = self.inner.with_log_file(log_file)?;
        Ok(self)
    }

    pub fn with_seq(mut self, key: impl Into<String>) -> Self {
        self.inner = self.inner.with_seq(key);
        self
    }

    pub fn log_with_time(&mut self, time: u64) -> bool {
        self.inner.log_with_time(time)
    }

    pub fn entry<K: Into<String>>(&mut self, key: K) -> Entry<'_, String, ScubaValue> {
        self.inner.entry(key)
    }

    pub fn flush(&self, timeout: Duration) {
        self.inner.flush(timeout)
    }

    pub fn get_sample(&self) -> &ScubaSample {
        self.inner.get_sample()
    }

    pub fn add_opt<K: Into<String>, V: Into<ScubaValue>>(
        &mut self,
        key: K,
        value: Option<V>,
    ) -> &mut Self {
        self.inner.add_opt(key, value);
        self
    }

    pub fn get<K: Into<String>>(&self, key: K) -> Option<&ScubaValue> {
        self.inner.get(key)
    }

    /// Set the [subset] of this sample.
    ///
    /// [subset]: https://fburl.com/qa/xqm9hsxx
    pub fn set_subset<S: Into<String>>(&mut self, subset: S) -> &mut Self {
        self.inner.set_subset(subset);
        self
    }
}

pub trait FutureStatsScubaExt {
    type Output;

    fn log_future_stats(
        self,
        scuba: MononokeScubaSampleBuilder,
        log_tag: &str,
        msg: impl Into<Option<String>>,
    ) -> Self::Output;
}

impl<T> FutureStatsScubaExt for (FutureStats, T) {
    type Output = T;

    fn log_future_stats(
        self,
        mut scuba: MononokeScubaSampleBuilder,
        log_tag: &str,
        msg: impl Into<Option<String>>,
    ) -> T {
        let (stats, res) = self;
        scuba.add_future_stats(&stats);
        scuba.log_with_msg(log_tag, msg);
        res
    }
}
