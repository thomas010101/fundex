//! Utilities to keep moving statistics about queries

use lazy_static::lazy_static;
use rand::{prelude::Rng, thread_rng};
use std::collections::{HashMap, HashSet};
use std::env;
use std::iter::FromIterator;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::components::metrics::{Counter, Gauge, MetricsRegistry};
use crate::components::store::PoolWaitStats;
use crate::data::graphql::shape_hash::shape_hash;
use crate::data::query::{CacheStatus, QueryExecutionError};
use crate::prelude::q;
use crate::prelude::{async_trait, debug, info, o, warn, CheapClone, Logger, QueryLoadManager};
use crate::util::stats::{MovingStats, BIN_SIZE, WINDOW_SIZE};

const ZERO_DURATION: Duration = Duration::from_millis(0);

lazy_static! {
    static ref LOAD_THRESHOLD: Duration = {
        let threshold = env::var("GRAPH_LOAD_THRESHOLD")
            .ok()
            .map(|s| {
                u64::from_str(&s).unwrap_or_else(|_| {
                    panic!("GRAPH_LOAD_THRESHOLD must be a number, but is `{}`", s)
                })
            })
            .unwrap_or(0);
        Duration::from_millis(threshold)
    };

    static ref JAIL_QUERIES: bool = env::var("GRAPH_LOAD_JAIL_THRESHOLD").is_ok();

    static ref JAIL_THRESHOLD: f64 = {
        env::var("GRAPH_LOAD_JAIL_THRESHOLD")
            .ok()
            .map(|s| {
                f64::from_str(&s).unwrap_or_else(|_| {
                    panic!("GRAPH_LOAD_JAIL_THRESHOLD must be a number, but is `{}`", s)
                })
            })
            .unwrap_or(1e9)
    };

    // Load management can be disabled by setting the threshold to 0. This
    // makes sure in particular that we never take any of the locks
    // associated with it
    static ref LOAD_MANAGEMENT_DISABLED: bool = *LOAD_THRESHOLD == ZERO_DURATION;

    static ref SIMULATE: bool = env::var("GRAPH_LOAD_SIMULATE").is_ok();

    // There is typically no need to configure this. But this can be used to effectivey disable the
    // semaphore by setting it to a high number.
    static ref EXTRA_QUERY_PERMITS: usize = {
        env::var("GRAPH_EXTRA_QUERY_PERMITS")
            .ok()
            .map(|s| {
                usize::from_str(&s).unwrap_or_else(|_| {
                    panic!("GRAPH_EXTRA_QUERY_PERMITS must be a number, but is `{}`", s)
                })
            })
            .unwrap_or(0)
    };
}

struct QueryEffort {
    inner: Arc<RwLock<QueryEffortInner>>,
}

/// Track the effort for queries (identified by their ShapeHash) over a
/// time window.
struct QueryEffortInner {
    window_size: Duration,
    bin_size: Duration,
    effort: HashMap<u64, MovingStats>,
    total: MovingStats,
}

/// Create a `QueryEffort` that uses the window and bin sizes configured in
/// the environment
impl Default for QueryEffort {
    fn default() -> Self {
        Self::new(*WINDOW_SIZE, *BIN_SIZE)
    }
}

impl QueryEffort {
    pub fn new(window_size: Duration, bin_size: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(QueryEffortInner::new(window_size, bin_size))),
        }
    }

    pub fn add(&self, shape_hash: u64, duration: Duration, gauge: &Box<Gauge>) {
        let mut inner = self.inner.write().unwrap();
        inner.add(shape_hash, duration);
        gauge.set(inner.total.average().unwrap_or(ZERO_DURATION).as_millis() as f64);
    }

    /// Return what we know right now about the effort for the query
    /// `shape_hash`, and about the total effort. If we have no measurements
    /// at all, return `ZERO_DURATION` as the total effort. If we have no
    /// data for the particular query, return `None` as the effort
    /// for the query
    pub fn current_effort(&self, shape_hash: u64) -> (Option<Duration>, Duration) {
        let inner = self.inner.read().unwrap();
        let total_effort = inner.total.duration();
        let query_effort = inner.effort.get(&shape_hash).map(|stats| stats.duration());
        (query_effort, total_effort)
    }
}

impl QueryEffortInner {
    fn new(window_size: Duration, bin_size: Duration) -> Self {
        Self {
            window_size,
            bin_size,
            effort: HashMap::default(),
            total: MovingStats::new(window_size, bin_size),
        }
    }

    fn add(&mut self, shape_hash: u64, duration: Duration) {
        let window_size = self.window_size;
        let bin_size = self.bin_size;
        let now = Instant::now();
        self.effort
            .entry(shape_hash)
            .or_insert_with(|| MovingStats::new(window_size, bin_size))
            .add_at(now, duration);
        self.total.add_at(now, duration);
    }
}

/// What to log about the state we are currently in
enum KillStateLogEvent {
    /// Overload is starting right now
    Start,
    /// Overload has been going on for the duration
    Ongoing(Duration),
    /// No longer overloaded, reducing the kill_rate
    Settling,
    /// Overload was resolved after duration time
    Resolved(Duration),
    /// Don't log anything right now
    Skip,
}

struct KillState {
    // A value between 0 and 1, where 0 means 'respond to all queries'
    // and 1 means 'do not respond to any queries'
    kill_rate: f64,
    // We adjust the `kill_rate` at most every `KILL_RATE_UPDATE_INTERVAL`
    last_update: Instant,
    // When the current overload situation started
    overload_start: Option<Instant>,
    // Throttle logging while we are overloaded to no more often than
    // once every 30s
    last_overload_log: Instant,
}

impl KillState {
    fn new() -> Self {
        // Set before to an instant long enough ago so that we don't
        // immediately log or adjust the kill rate if the node is already
        // under load. Unfortunately, on OSX, `Instant` measures time from
        // the last boot, and if that was less than 60s ago, we can't
        // subtract 60s from `now`. Since the worst that can happen if
        // we set `before` to `now` is that we might log more than strictly
        // necessary, and adjust the kill rate one time too often right after
        // node start, it is acceptable to fall back to `now`
        let before = {
            let long_ago = Duration::from_secs(60);
            let now = Instant::now();
            now.checked_sub(long_ago).unwrap_or(now)
        };
        Self {
            kill_rate: 0.0,
            last_update: before,
            overload_start: None,
            last_overload_log: before,
        }
    }

    fn log_event(&mut self, now: Instant, kill_rate: f64, overloaded: bool) -> KillStateLogEvent {
        use KillStateLogEvent::*;

        if let Some(overload_start) = self.overload_start {
            if !overloaded {
                if kill_rate == 0.0 {
                    self.overload_start = None;
                    Resolved(overload_start.elapsed())
                } else {
                    Settling
                }
            } else if now.saturating_duration_since(self.last_overload_log)
                > Duration::from_secs(30)
            {
                self.last_overload_log = now;
                Ongoing(overload_start.elapsed())
            } else {
                Skip
            }
        } else if overloaded {
            self.overload_start = Some(now);
            self.last_overload_log = now;
            Start
        } else {
            Skip
        }
    }
}

/// Indicate what the load manager wants query execution to do with a query
#[derive(Debug, Clone, Copy)]
pub enum Decision {
    /// Proceed with executing the query
    Proceed,
    /// The query is too expensive and should not be executed
    TooExpensive,
    /// The service is overloaded, and we should not execute the query
    /// right now
    Throttle,
}

impl Decision {
    pub fn to_result(self) -> Result<(), QueryExecutionError> {
        use Decision::*;
        match self {
            Proceed => Ok(()),
            TooExpensive => Err(QueryExecutionError::TooExpensive),
            Throttle => Err(QueryExecutionError::Throttled),
        }
    }
}

pub struct LoadManager {
    logger: Logger,
    effort: QueryEffort,
    blocked_queries: HashSet<u64>,
    jailed_queries: RwLock<HashSet<u64>>,
    kill_state: RwLock<KillState>,
    effort_gauge: Box<Gauge>,
    query_counters: HashMap<CacheStatus, Counter>,

    query_semaphore: Arc<tokio::sync::Semaphore>,
    semaphore_wait_stats: RwLock<MovingStats>,
    semaphore_wait_gauge: Box<Gauge>,
}

impl LoadManager {
    pub fn new(
        logger: &Logger,
        blocked_queries: Vec<Arc<q::Document>>,
        registry: Arc<dyn MetricsRegistry>,
        store_conn_pool_size: usize,
    ) -> Self {
        let logger = logger.new(o!("component" => "LoadManager"));
        let blocked_queries = blocked_queries
            .into_iter()
            .map(|doc| shape_hash(&doc))
            .collect::<HashSet<_>>();

        let mode = if *LOAD_MANAGEMENT_DISABLED {
            "disabled"
        } else if *SIMULATE {
            "simulation"
        } else {
            "enabled"
        };
        info!(logger, "Creating LoadManager in {} mode", mode,);

        let effort_gauge = registry
            .new_gauge(
                "query_effort_ms",
                "Moving average of time spent running queries",
                HashMap::new(),
            )
            .expect("failed to create `query_effort_ms` counter");
        let query_counters = CacheStatus::iter()
            .map(|s| {
                let labels = HashMap::from_iter(vec![("cache_status".to_owned(), s.to_string())]);
                let counter = registry
                    .global_counter(
                        "query_cache_status_count",
                        "Count toplevel GraphQL fields executed and their cache status",
                        labels,
                    )
                    .expect("Failed to register query_counter metric");
                (s.clone(), counter)
            })
            .collect::<HashMap<_, _>>();

        let semaphore_wait_gauge = registry
            .new_gauge(
                "query_semaphore_wait_ms",
                "Moving average of time spent running queries",
                HashMap::new(),
            )
            .expect("failed to create `query_effort_ms` counter");

        // A query is always consuming a CPU core, or a DB connection, or both.
        // So if more than `store_conn_pool_size + num_cpus::get()` queries are executing,
        // there will be contention for resources.
        let max_concurrent_queries = store_conn_pool_size + num_cpus::get() + *EXTRA_QUERY_PERMITS;
        let query_semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent_queries));
        Self {
            logger,
            effort: QueryEffort::default(),
            blocked_queries,
            jailed_queries: RwLock::new(HashSet::new()),
            kill_state: RwLock::new(KillState::new()),
            effort_gauge,
            query_counters,
            query_semaphore,
            semaphore_wait_stats: RwLock::new(MovingStats::default()),
            semaphore_wait_gauge,
        }
    }

    /// Record that we spent `duration` amount of work for the query
    /// `shape_hash`, where `cache_status` indicates whether the query
    /// was cached or had to actually run
    pub fn record_work(&self, shape_hash: u64, duration: Duration, cache_status: CacheStatus) {
        self.query_counters
            .get(&cache_status)
            .map(|counter| counter.inc());
        if !*LOAD_MANAGEMENT_DISABLED {
            self.effort.add(shape_hash, duration, &self.effort_gauge);
        }
    }

    pub fn decide(&self, wait_stats: &PoolWaitStats, shape_hash: u64, query: &str) -> Decision {
        use Decision::*;

        if self.blocked_queries.contains(&shape_hash) {
            return TooExpensive;
        }
        if *LOAD_MANAGEMENT_DISABLED {
            return Proceed;
        }

        if self.jailed_queries.read().unwrap().contains(&shape_hash) {
            return if *SIMULATE { Proceed } else { TooExpensive };
        }

        let (overloaded, wait_ms) = self.overloaded(wait_stats);
        let (kill_rate, last_update) = self.kill_state();
        if !overloaded && kill_rate == 0.0 {
            return Proceed;
        }

        let (query_effort, total_effort) = self.effort.current_effort(shape_hash);
        // When `total_effort` is `ZERO_DURATION`, we haven't done any work. All are
        // welcome
        if total_effort == ZERO_DURATION {
            return Proceed;
        }

        // If `query_effort` is `None`, we haven't seen the query. Since we
        // are in an overload situation, we are very suspicious of new things
        // and assume the worst. This ensures that even if we only ever see
        // new queries, we drop `kill_rate` amount of traffic
        let known_query = query_effort.is_some();
        let query_effort = query_effort.unwrap_or_else(|| total_effort).as_millis() as f64;
        let total_effort = total_effort.as_millis() as f64;

        if known_query && *JAIL_QUERIES && query_effort / total_effort > *JAIL_THRESHOLD {
            // Any single query that causes at least JAIL_THRESHOLD of the
            // effort in an overload situation gets killed
            warn!(self.logger, "Jailing query";
                "query" => query,
                "wait_ms" => wait_ms.as_millis(),
                "query_effort_ms" => query_effort,
                "total_effort_ms" => total_effort,
                "ratio" => format!("{:.4}", query_effort/total_effort));
            self.jailed_queries.write().unwrap().insert(shape_hash);
            return if *SIMULATE { Proceed } else { TooExpensive };
        }

        // Kill random queries in case we have no queries, or not enough queries
        // that cause at least 20% of the effort
        let kill_rate = self.update_kill_rate(kill_rate, last_update, overloaded, wait_ms);
        let decline =
            thread_rng().gen_bool((kill_rate * query_effort / total_effort).min(1.0).max(0.0));
        if decline {
            if *SIMULATE {
                debug!(self.logger, "Declining query";
                    "query" => query,
                    "wait_ms" => wait_ms.as_millis(),
                    "query_weight" => format!("{:.2}", query_effort / total_effort),
                    "kill_rate" => format!("{:.4}", kill_rate),
                );
                return Proceed;
            } else {
                return Throttle;
            }
        }
        Proceed
    }

    fn overloaded(&self, wait_stats: &PoolWaitStats) -> (bool, Duration) {
        let store_avg = wait_stats.read().unwrap().average();
        let semaphore_avg = self.semaphore_wait_stats.read().unwrap().average();
        let max_avg = store_avg.max(semaphore_avg);
        let overloaded = max_avg
            .map(|average| average > *LOAD_THRESHOLD)
            .unwrap_or(false);
        (overloaded, max_avg.unwrap_or(ZERO_DURATION))
    }

    fn kill_state(&self) -> (f64, Instant) {
        let state = self.kill_state.read().unwrap();
        (state.kill_rate, state.last_update)
    }

    fn update_kill_rate(
        &self,
        mut kill_rate: f64,
        last_update: Instant,
        overloaded: bool,
        wait_ms: Duration,
    ) -> f64 {
        // The rates by which we increase and decrease the `kill_rate`; when
        // we increase the `kill_rate`, we do that in a way so that we do drop
        // fewer queries as the `kill_rate` approaches 1.0. After `n`
        // consecutive steps of increasing the `kill_rate`, it will
        // be `1 - (1-KILL_RATE_STEP_UP)^n`
        //
        // When we step down, we do that in fixed size steps to move away from
        // dropping queries fairly quickly so that after `n` steps of reducing
        // the `kill_rate`, it is at most `1 - n * KILL_RATE_STEP_DOWN`
        //
        // The idea behind this is that we want to be conservative when we drop
        // queries, but aggressive when we reduce the amount of queries we drop
        // to disrupt traffic for as little as possible.
        const KILL_RATE_STEP_UP: f64 = 0.1;
        const KILL_RATE_STEP_DOWN: f64 = 2.0 * KILL_RATE_STEP_UP;
        const KILL_RATE_UPDATE_INTERVAL: Duration = Duration::from_millis(1000);

        assert!(overloaded || kill_rate > 0.0);

        let now = Instant::now();
        if now.saturating_duration_since(last_update) > KILL_RATE_UPDATE_INTERVAL {
            // Update the kill_rate
            if overloaded {
                kill_rate = (kill_rate + KILL_RATE_STEP_UP * (1.0 - kill_rate)).min(1.0);
            } else {
                kill_rate = (kill_rate - KILL_RATE_STEP_DOWN).max(0.0);
            }
            let event = {
                let mut state = self.kill_state.write().unwrap();
                state.kill_rate = kill_rate;
                state.last_update = now;
                state.log_event(now, kill_rate, overloaded)
            };
            // Log information about what's happening after we've released the
            // lock on self.kill_state
            use KillStateLogEvent::*;
            match event {
                Settling => {
                    info!(self.logger, "Query overload improving";
                        "wait_ms" => wait_ms.as_millis(),
                        "kill_rate" => format!("{:.4}", kill_rate),
                        "event" => "settling");
                }
                Resolved(duration) => {
                    info!(self.logger, "Query overload resolved";
                        "duration_ms" => duration.as_millis(),
                        "wait_ms" => wait_ms.as_millis(),
                        "event" => "resolved");
                }
                Ongoing(duration) => {
                    info!(self.logger, "Query overload still happening";
                        "duration_ms" => duration.as_millis(),
                        "wait_ms" => wait_ms.as_millis(),
                        "kill_rate" => format!("{:.4}", kill_rate),
                        "event" => "ongoing");
                }
                Start => {
                    warn!(self.logger, "Query overload";
                    "wait_ms" => wait_ms.as_millis(),
                    "event" => "start");
                }
                Skip => { /* do nothing */ }
            }
        }
        kill_rate
    }

    fn add_wait_time(&self, duration: Duration) {
        let wait_avg = {
            let mut wait_stats = self.semaphore_wait_stats.write().unwrap();
            wait_stats.add(duration);
            wait_stats.average()
        };
        if let Some(wait_avg) = wait_avg.map(|wait_avg| wait_avg.as_millis()) {
            self.semaphore_wait_gauge.set(wait_avg as f64);
        }
    }
}

#[async_trait]
impl QueryLoadManager for LoadManager {
    async fn query_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        let start = Instant::now();
        let permit = self.query_semaphore.cheap_clone().acquire_owned().await;
        self.add_wait_time(start.elapsed());
        permit
    }

    fn record_work(&self, shape_hash: u64, duration: Duration, cache_status: CacheStatus) {
        self.query_counters
            .get(&cache_status)
            .map(|counter| counter.inc());
        if !*LOAD_MANAGEMENT_DISABLED {
            self.effort.add(shape_hash, duration, &self.effort_gauge);
        }
    }
}
