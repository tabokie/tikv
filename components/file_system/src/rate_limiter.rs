// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use super::metrics::RATE_LIMITER_REQUEST_WAIT_DURATION;
use super::{IOOp, IOPriority, IOType};

#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use crossbeam_utils::CachePadded;
use parking_lot::Mutex;
use strum::EnumCount;
use tikv_util::time::Instant;
use tikv_util::worker::Worker;

/// Record accumulated bytes through of different types.
/// Used for testing and metrics.
#[derive(Debug)]
pub struct IORateLimiterStatistics {
    read_bytes: [CachePadded<AtomicUsize>; IOType::COUNT],
    write_bytes: [CachePadded<AtomicUsize>; IOType::COUNT],
}

impl IORateLimiterStatistics {
    pub fn new() -> Self {
        IORateLimiterStatistics {
            read_bytes: Default::default(),
            write_bytes: Default::default(),
        }
    }

    pub fn fetch(&self, io_type: IOType, io_op: IOOp) -> usize {
        let io_type_idx = io_type as usize;
        match io_op {
            IOOp::Read => self.read_bytes[io_type_idx].load(Ordering::Relaxed),
            IOOp::Write => self.write_bytes[io_type_idx].load(Ordering::Relaxed),
        }
    }

    pub fn record(&self, io_type: IOType, io_op: IOOp, bytes: usize) {
        let io_type_idx = io_type as usize;
        match io_op {
            IOOp::Read => {
                self.read_bytes[io_type_idx].fetch_add(bytes, Ordering::Relaxed);
            }
            IOOp::Write => {
                self.write_bytes[io_type_idx].fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    pub fn reset(&self) {
        for i in 0..IOType::COUNT {
            self.read_bytes[i].store(0, Ordering::Relaxed);
            self.write_bytes[i].store(0, Ordering::Relaxed);
        }
    }
}

macro_rules! do_sleep {
    ($duration:expr, sync) => {
        std::thread::sleep($duration)
    };
    ($duration:expr, async) => {
        tokio::time::delay_for($duration).await
    };
}

const DEFAULT_REFILL_PERIOD: Duration = Duration::from_millis(40);

/// Limit total IO flow below provided threshold by throttling lower-priority IOs.
/// Rate limit is disabled when total IO threshold is set to zero.
#[derive(Debug)]
struct PriorityBasedIORateLimiter {
    // IO amount passed through within current epoch
    bytes_through: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    // Maximum IOs permitted within current epoch
    bytes_per_epoch: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    protected: Mutex<PriorityBasedIORateLimiterProtected>,
}

#[derive(Debug)]
struct PriorityBasedIORateLimiterProtected {
    next_refill_time: Instant,
    // IO amount that is drew from the next epoch in advance
    pending_bytes: [usize; IOPriority::COUNT],
    // estimated throughput of recent epochs
    estimated_bytes_through: [IOThroughputEstimator; IOPriority::COUNT],
}

impl PriorityBasedIORateLimiterProtected {
    fn new() -> Self {
        PriorityBasedIORateLimiterProtected {
            next_refill_time: Instant::now_coarse() + DEFAULT_REFILL_PERIOD,
            pending_bytes: [0; IOPriority::COUNT],
            estimated_bytes_through: [IOThroughputEstimator::new(); IOPriority::COUNT],
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct IOThroughputEstimator {
    /// total count of sampled epochs
    count: usize,
    /// sum of IOs amount over a window
    sum: usize,
}

impl IOThroughputEstimator {
    fn new() -> IOThroughputEstimator {
        IOThroughputEstimator { count: 0, sum: 0 }
    }

    fn maybe_update_estimation(&mut self, v: usize) -> Option<usize> {
        const WINDOW_SIZE: usize = 5;
        self.count += 1;
        self.sum += v;
        // average over DEFAULT_REFILL_PERIOD*WINDOW_SIZE=200ms window
        if self.count % WINDOW_SIZE == 0 {
            let avg = self.sum / WINDOW_SIZE;
            self.sum = 0;
            Some(avg)
        } else {
            None
        }
    }
}

/// Actual implementation for requesting IOs from PriorityBasedIORateLimiter.
/// An attempt will be recorded first. If the attempted amount exceeds the available quotas of
/// current epoch, the requester will register itself and sleep until next epoch.
macro_rules! request_imp {
    ($self:ident, $priority:ident, $amount:ident, $mode:tt) => {{
        let priority_idx = $priority as usize;
        loop {
            let cached_bytes_per_refill =
                $self.bytes_per_epoch[priority_idx].load(Ordering::Relaxed);
            if cached_bytes_per_refill == 0 {
                return $amount;
            }
            let amount = std::cmp::min($amount, cached_bytes_per_refill);
            let bytes_through =
                $self.bytes_through[priority_idx].fetch_add(amount, Ordering::AcqRel) + amount;
            if bytes_through <= cached_bytes_per_refill {
                return amount;
            }
            let now = Instant::now_coarse();
            let (next_refill_time, pending) = {
                let mut locked = $self.protected.lock();
                // a small delay in case a refill slips in after `bytes_per_epoch` was fetched.
                if locked.next_refill_time + Duration::from_millis(1) >= now + DEFAULT_REFILL_PERIOD
                {
                    continue;
                }
                locked.pending_bytes[priority_idx] += amount;
                (locked.next_refill_time, locked.pending_bytes[priority_idx])
            };
            let mut wait = DEFAULT_REFILL_PERIOD * (pending / cached_bytes_per_refill) as u32;
            if next_refill_time > now {
                // limit update is infrequent, let's assume it won't happen during the sleep
                wait += next_refill_time - now;
            } else if next_refill_time + DEFAULT_REFILL_PERIOD / 2 < now {
                // expected refill delayed too long
                $self.refill();
            }
            RATE_LIMITER_REQUEST_WAIT_DURATION
                .with_label_values(&[$priority.as_str()])
                .observe(wait.as_secs_f64());
            do_sleep!(wait, $mode);
            return amount;
        }
    }};
}

impl PriorityBasedIORateLimiter {
    fn new() -> Self {
        PriorityBasedIORateLimiter {
            bytes_through: Default::default(),
            bytes_per_epoch: Default::default(),
            protected: Mutex::new(PriorityBasedIORateLimiterProtected::new()),
        }
    }

    /// Dynamically changes the total IO flow threshold, effective after at most
    /// `DEFAULT_REFILL_PERIOD`.
    #[allow(dead_code)]
    fn set_bytes_per_sec(&self, bytes_per_sec: usize) {
        let now = (bytes_per_sec as f64 * DEFAULT_REFILL_PERIOD.as_secs_f64()) as usize;
        let before = self.bytes_per_epoch[IOPriority::High as usize].swap(now, Ordering::Relaxed);
        if before == 0 || now == 0 {
            // toggle on/off rate limit.
            // we hold this lock so a concurrent refill can't negate our effort.
            let _locked = self.protected.lock();
            for p in &[IOPriority::Medium, IOPriority::Low] {
                let pi = *p as usize;
                self.bytes_per_epoch[pi].store(now, Ordering::Relaxed);
            }
        }
    }

    fn request(&self, priority: IOPriority, amount: usize) -> usize {
        request_imp!(self, priority, amount, sync)
    }

    async fn async_request(&self, priority: IOPriority, amount: usize) -> usize {
        request_imp!(self, priority, amount, async)
    }

    /// Called by a daemon thread every `DEFAULT_REFILL_PERIOD`.
    /// It is done so because the algorithm correctness relies on refill epoch being
    /// faithful to physical time.
    fn refill(&self) {
        let mut locked = self.protected.lock();

        let mut limit = self.bytes_per_epoch[IOPriority::High as usize].load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let now = Instant::now_coarse();
        if locked.next_refill_time > now + DEFAULT_REFILL_PERIOD / 2 {
            // already refilled
            return;
        }

        // keep in sync with a potentially skewed clock
        locked.next_refill_time = now + DEFAULT_REFILL_PERIOD;

        debug_assert!(IOPriority::High as usize > IOPriority::Medium as usize);
        for p in &[IOPriority::High, IOPriority::Medium] {
            let pi = *p as usize;
            // reset IO consuption
            let bytes_through = std::cmp::min(
                self.bytes_through[pi].swap(locked.pending_bytes[pi], Ordering::Release),
                limit,
            );
            // pending IOs are inherited across epochs
            locked.pending_bytes[pi] = locked.pending_bytes[pi].saturating_sub(limit);
            // calibrate and update IO quotas for next lower priority
            if let Some(bytes_through) =
                locked.estimated_bytes_through[pi].maybe_update_estimation(bytes_through)
            {
                limit = if limit > bytes_through {
                    limit - bytes_through
                } else {
                    1 // a small positive value
                };
                self.bytes_per_epoch[pi - 1].store(limit, Ordering::Relaxed);
            } else {
                limit = self.bytes_per_epoch[pi - 1].load(Ordering::Relaxed);
            }
        }
        self.bytes_through[IOPriority::Low as usize].store(
            locked.pending_bytes[IOPriority::Low as usize],
            Ordering::Release,
        );
        locked.pending_bytes[IOPriority::Low as usize] =
            locked.pending_bytes[IOPriority::Low as usize].saturating_sub(limit);
    }
}

/// An instance of `IORateLimiter` should be safely shared between threads.
#[derive(Debug)]
pub struct IORateLimiter {
    priority_map: [IOPriority; IOType::COUNT],
    throughput_limiter: Arc<PriorityBasedIORateLimiter>,
    stats: Option<Arc<IORateLimiterStatistics>>,
}

impl IORateLimiter {
    pub fn new(enable_statistics: bool) -> IORateLimiter {
        IORateLimiter {
            priority_map: [IOPriority::High; IOType::COUNT],
            throughput_limiter: Arc::new(PriorityBasedIORateLimiter::new()),
            stats: if enable_statistics {
                Some(Arc::new(IORateLimiterStatistics::new()))
            } else {
                None
            },
        }
    }

    pub fn set_io_priority(&mut self, io_type: IOType, io_priority: IOPriority) {
        self.priority_map[io_type as usize] = io_priority;
    }

    pub fn statistics(&self) -> Option<Arc<IORateLimiterStatistics>> {
        self.stats.clone()
    }

    pub fn set_io_rate_limit(&self, rate: usize) {
        self.throughput_limiter.set_bytes_per_sec(rate);
    }

    pub fn refill(&self) {
        self.throughput_limiter.refill();
    }

    /// Requests for token for bytes and potentially update statistics. If this
    /// request can not be satisfied, the call is blocked. Granted token can be
    /// less than the requested bytes, but must be greater than zero.
    pub fn request(&self, io_type: IOType, io_op: IOOp, mut bytes: usize) -> usize {
        if io_op == IOOp::Write {
            let priority = self.priority_map[io_type as usize];
            bytes = self.throughput_limiter.request(priority, bytes);
        }
        if let Some(stats) = &self.stats {
            stats.record(io_type, io_op, bytes);
        }
        bytes
    }

    /// Asynchronously requests for token for bytes and potentially update
    /// statistics. If this request can not be satisfied, the call is blocked.
    /// Granted token can be less than the requested bytes, but must be greater
    /// than zero.
    pub async fn async_request(&self, io_type: IOType, io_op: IOOp, mut bytes: usize) -> usize {
        if io_op == IOOp::Write {
            let priority = self.priority_map[io_type as usize];
            bytes = self.throughput_limiter.async_request(priority, bytes).await;
        }
        if let Some(stats) = &self.stats {
            stats.record(io_type, io_op, bytes);
        }
        bytes
    }
}

lazy_static! {
    static ref IO_RATE_LIMITER: Mutex<Option<Arc<IORateLimiter>>> = Mutex::new(None);
}

// Do NOT use this method in test environment.
pub fn set_io_rate_limiter(limiter: Option<Arc<IORateLimiter>>) {
    *IO_RATE_LIMITER.lock() = limiter;
}

pub fn get_io_rate_limiter() -> Option<Arc<IORateLimiter>> {
    if let Some(ref limiter) = *IO_RATE_LIMITER.lock() {
        Some(limiter.clone())
    } else {
        None
    }
}

pub fn start_global_io_rate_limiter_daemon(worker: &Worker) {
    worker.spawn_interval_task(DEFAULT_REFILL_PERIOD, move || {
        if let Some(limiter) = get_io_rate_limiter() {
            limiter.refill();
        }
    });
}

#[cfg(test)]
pub struct LocalIORateLimiterDaemon {
    _thread: std::thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

#[cfg(test)]
impl Drop for LocalIORateLimiterDaemon {
    fn drop(&mut self) {
        self.stop.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub fn start_local_io_rate_limiter_daemon(limiter: Arc<IORateLimiter>) -> LocalIORateLimiterDaemon {
    let stop = Arc::new(AtomicBool::new(false));
    let stop1 = stop.clone();
    LocalIORateLimiterDaemon {
        _thread: std::thread::spawn(move || {
            while !stop1.load(Ordering::Relaxed) {
                limiter.refill();
                std::thread::sleep(DEFAULT_REFILL_PERIOD);
            }
        }),
        stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approximate_eq(left: f64, right: f64) {
        assert!(left >= right * 0.9);
        assert!(left <= right * 1.1);
    }

    struct BackgroundContext {
        threads: Vec<std::thread::JoinHandle<()>>,
        stop: Option<Arc<AtomicBool>>,
    }

    impl Drop for BackgroundContext {
        fn drop(&mut self) {
            if let Some(stop) = &self.stop {
                stop.store(true, Ordering::Relaxed);
            }
            for t in self.threads.drain(..) {
                t.join().unwrap();
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Request(IOType, IOOp, usize);

    fn start_background_jobs(
        limiter: &Arc<IORateLimiter>,
        job_count: usize,
        request: Request,
        interval: Option<Duration>,
    ) -> BackgroundContext {
        let mut threads = vec![];
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..job_count {
            let stop = stop.clone();
            let limiter = limiter.clone();
            let t = std::thread::spawn(move || {
                let Request(io_type, op, len) = request;
                while !stop.load(Ordering::Relaxed) {
                    limiter.request(io_type, op, len);
                    if let Some(interval) = interval {
                        std::thread::sleep(interval);
                    }
                }
            });
            threads.push(t);
        }
        BackgroundContext {
            threads,
            stop: Some(stop),
        }
    }

    fn verify_rate_limit(limiter: &Arc<IORateLimiter>, bytes_per_sec: usize) {
        let stats = limiter.statistics().unwrap();
        stats.reset();
        limiter.set_io_rate_limit(bytes_per_sec);
        let duration = {
            let begin = Instant::now();
            {
                let _context = start_background_jobs(
                    limiter,
                    10, /*job_count*/
                    Request(IOType::ForegroundWrite, IOOp::Write, 10),
                    None, /*interval*/
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        approximate_eq(
            stats.fetch(IOType::ForegroundWrite, IOOp::Write) as f64,
            bytes_per_sec as f64 * duration.as_secs_f64(),
        );
    }

    #[test]
    fn test_rate_limited_heavy_flow() {
        let low_bytes_per_sec = 2000;
        let high_bytes_per_sec = 10000;
        let limiter = Arc::new(IORateLimiter::new(true /*enable_statistics*/));
        let _deamon = start_local_io_rate_limiter_daemon(limiter.clone());
        verify_rate_limit(&limiter, low_bytes_per_sec);
        verify_rate_limit(&limiter, high_bytes_per_sec);
        verify_rate_limit(&limiter, low_bytes_per_sec);
    }

    #[test]
    fn test_rate_limited_light_flow() {
        let kbytes_per_sec = 3;
        let actual_kbytes_per_sec = 2;
        let limiter = Arc::new(IORateLimiter::new(true /*enable_statistics*/));
        limiter.set_io_rate_limit(kbytes_per_sec * 1000);
        let stats = limiter.statistics().unwrap();
        let _deamon = start_local_io_rate_limiter_daemon(limiter.clone());
        let duration = {
            let begin = Instant::now();
            {
                // each thread request at most 1000 bytes per second
                let _context = start_background_jobs(
                    &limiter,
                    actual_kbytes_per_sec, /*job_count*/
                    Request(IOType::Compaction, IOOp::Write, 1),
                    Some(Duration::from_millis(1)),
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        approximate_eq(
            stats.fetch(IOType::Compaction, IOOp::Write) as f64,
            actual_kbytes_per_sec as f64 * duration.as_secs_f64() * 1000.0,
        );
    }

    #[test]
    fn test_rate_limited_hybrid_flow() {
        let bytes_per_sec = 100000;
        let write_work = 50;
        let compaction_work = 60;
        let import_work = 10;
        let mut limiter = IORateLimiter::new(true /*enable_statistics*/);
        limiter.set_io_rate_limit(bytes_per_sec);
        limiter.set_io_priority(IOType::Compaction, IOPriority::Medium);
        limiter.set_io_priority(IOType::Import, IOPriority::Low);
        let stats = limiter.statistics().unwrap();
        let limiter = Arc::new(limiter);
        let _deamon = start_local_io_rate_limiter_daemon(limiter.clone());
        let duration = {
            let begin = Instant::now();
            {
                let _write = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::ForegroundWrite,
                        IOOp::Write,
                        write_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                let _compaction = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::Compaction,
                        IOOp::Write,
                        compaction_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                let _import = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::Import,
                        IOOp::Write,
                        import_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        let write_bytes = stats.fetch(IOType::ForegroundWrite, IOOp::Write);
        approximate_eq(
            write_bytes as f64,
            (write_work * bytes_per_sec / 100) as f64 * duration.as_secs_f64(),
        );
        let compaction_bytes = stats.fetch(IOType::Compaction, IOOp::Write);
        approximate_eq(
            compaction_bytes as f64,
            ((100 - write_work) * bytes_per_sec / 100) as f64 * duration.as_secs_f64(),
        );
        let import_bytes = stats.fetch(IOType::Import, IOOp::Write);
        let total_bytes = write_bytes + import_bytes + compaction_bytes;
        approximate_eq(
            total_bytes as f64,
            bytes_per_sec as f64 * duration.as_secs_f64(),
        );
    }
}
