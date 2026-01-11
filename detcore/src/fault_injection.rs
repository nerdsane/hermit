/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * Copyright (c) 2026 Bloodhound Authors.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic fault injection for DST (Deterministic Simulation Testing).
//!
//! This module provides BUGGIFY-style fault injection that is:
//! - Deterministic: Same seed always produces same fault sequence
//! - Configurable: Per-syscall-type fault probabilities
//! - Schedulable: Faults can be injected at specific logical times
//!
//! # Design
//!
//! The FaultInjector uses a PCG (Permuted Congruential Generator) for
//! deterministic random number generation. Each fault check consumes
//! exactly one RNG value to maintain determinism.
//!
//! # Usage
//!
//! ```ignore
//! let mut fi = FaultInjector::new(seed);
//! fi.set_probability(FaultType::DiskWrite, 0.1);
//!
//! // In syscall handler:
//! if fi.should_inject_fault(FaultType::DiskWrite, "task_name") {
//!     return Err(Errno::EIO);
//! }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};

/// Types of faults that can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FaultType {
    /// Disk write failure (EIO on write/pwrite/writev)
    DiskWrite,
    /// Disk read failure (EIO on read/pread/readv)
    DiskRead,
    /// Disk fsync failure (EIO on fsync/fdatasync)
    DiskFsync,
    /// Network connect refused (ECONNREFUSED)
    NetworkConnectRefused,
    /// Network connect timeout (ETIMEDOUT)
    NetworkConnectTimeout,
    /// Network bind failure (EADDRINUSE)
    NetworkBindFailed,
    /// Network listen failure (EADDRINUSE)
    NetworkListenFailed,
    /// Network accept failure (ECONNABORTED)
    NetworkAcceptFailed,
    /// Network send failure (EPIPE/ECONNRESET)
    NetworkSendFailed,
    /// Network recv failure (ECONNRESET)
    NetworkRecvFailed,
    /// Memory allocation failure (ENOMEM)
    MemoryAlloc,
    /// Process fork failure (EAGAIN)
    ForkFailed,
}

impl FaultType {
    /// Get all fault types for iteration.
    pub fn all() -> &'static [FaultType] {
        &[
            FaultType::DiskWrite,
            FaultType::DiskRead,
            FaultType::DiskFsync,
            FaultType::NetworkConnectRefused,
            FaultType::NetworkConnectTimeout,
            FaultType::NetworkBindFailed,
            FaultType::NetworkListenFailed,
            FaultType::NetworkAcceptFailed,
            FaultType::NetworkSendFailed,
            FaultType::NetworkRecvFailed,
            FaultType::MemoryAlloc,
            FaultType::ForkFailed,
        ]
    }

    /// Get the default errno to return for this fault type.
    pub fn default_errno(&self) -> i32 {
        match self {
            FaultType::DiskWrite => libc::EIO,
            FaultType::DiskRead => libc::EIO,
            FaultType::DiskFsync => libc::EIO,
            FaultType::NetworkConnectRefused => libc::ECONNREFUSED,
            FaultType::NetworkConnectTimeout => libc::ETIMEDOUT,
            FaultType::NetworkBindFailed => libc::EADDRINUSE,
            FaultType::NetworkListenFailed => libc::EADDRINUSE,
            FaultType::NetworkAcceptFailed => libc::ECONNABORTED,
            FaultType::NetworkSendFailed => libc::EPIPE,
            FaultType::NetworkRecvFailed => libc::ECONNRESET,
            FaultType::MemoryAlloc => libc::ENOMEM,
            FaultType::ForkFailed => libc::EAGAIN,
        }
    }
}

impl std::fmt::Display for FaultType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultType::DiskWrite => write!(f, "disk_write"),
            FaultType::DiskRead => write!(f, "disk_read"),
            FaultType::DiskFsync => write!(f, "disk_fsync"),
            FaultType::NetworkConnectRefused => write!(f, "network_connect_refused"),
            FaultType::NetworkConnectTimeout => write!(f, "network_connect_timeout"),
            FaultType::NetworkBindFailed => write!(f, "network_bind_failed"),
            FaultType::NetworkListenFailed => write!(f, "network_listen_failed"),
            FaultType::NetworkAcceptFailed => write!(f, "network_accept_failed"),
            FaultType::NetworkSendFailed => write!(f, "network_send_failed"),
            FaultType::NetworkRecvFailed => write!(f, "network_recv_failed"),
            FaultType::MemoryAlloc => write!(f, "memory_alloc"),
            FaultType::ForkFailed => write!(f, "fork_failed"),
        }
    }
}

/// A scheduled fault to be injected at a specific time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledFault {
    /// Logical time (nanoseconds) at which to inject the fault.
    pub time_ns: u64,
    /// Type of fault to inject.
    pub fault_type: FaultType,
    /// Optional target filter (task name, path, etc.).
    pub target: Option<String>,
    /// Whether this scheduled fault has been consumed.
    pub consumed: bool,
}

/// Result of a fault check.
#[derive(Debug, Clone)]
pub struct FaultCheckResult {
    /// Whether a fault should be injected.
    pub should_fault: bool,
    /// The type of fault if one should be injected.
    pub fault_type: Option<FaultType>,
    /// The errno to return.
    pub errno: Option<i32>,
    /// Debug message for logging.
    pub message: Option<String>,
}

impl FaultCheckResult {
    /// Create a result indicating no fault.
    pub fn no_fault() -> Self {
        Self {
            should_fault: false,
            fault_type: None,
            errno: None,
            message: None,
        }
    }

    /// Create a result indicating a fault should be injected.
    pub fn fault(fault_type: FaultType, message: impl Into<String>) -> Self {
        Self {
            should_fault: true,
            fault_type: Some(fault_type),
            errno: Some(fault_type.default_errno()),
            message: Some(message.into()),
        }
    }
}

/// Deterministic fault injector using PCG RNG.
///
/// Thread-safe through internal synchronization.
#[derive(Debug)]
pub struct FaultInjector {
    /// Deterministic RNG seeded from the main seed.
    rng: Pcg64Mcg,
    /// Whether fault injection is enabled.
    enabled: bool,
    /// Per-fault-type probabilities (0.0 to 1.0).
    probabilities: HashMap<FaultType, f64>,
    /// Scheduled faults by logical time.
    scheduled_faults: BTreeMap<u64, Vec<ScheduledFault>>,
    /// Total faults injected (for stats).
    faults_injected_count: AtomicU64,
    /// Total fault checks performed.
    fault_checks_count: AtomicU64,
    /// File descriptors to skip (stdin/stdout/stderr).
    protected_fds: Vec<i32>,
}

impl FaultInjector {
    /// Create a new FaultInjector with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Pcg64Mcg::seed_from_u64(seed),
            enabled: false,
            probabilities: HashMap::new(),
            scheduled_faults: BTreeMap::new(),
            faults_injected_count: AtomicU64::new(0),
            fault_checks_count: AtomicU64::new(0),
            protected_fds: vec![0, 1, 2], // stdin, stdout, stderr
        }
    }

    /// Enable or disable fault injection.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        debug!("FaultInjector enabled: {}", enabled);
    }

    /// Check if fault injection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Set the probability for a specific fault type.
    ///
    /// # Arguments
    /// * `fault_type` - The type of fault.
    /// * `probability` - Probability between 0.0 and 1.0.
    pub fn set_probability(&mut self, fault_type: FaultType, probability: f64) {
        let prob = probability.clamp(0.0, 1.0);
        if prob > 0.0 {
            self.probabilities.insert(fault_type, prob);
            self.enabled = true;
            debug!("FaultInjector: {} probability set to {:.4}", fault_type, prob);
        } else {
            self.probabilities.remove(&fault_type);
        }
    }

    /// Get the probability for a specific fault type.
    pub fn get_probability(&self, fault_type: FaultType) -> f64 {
        *self.probabilities.get(&fault_type).unwrap_or(&0.0)
    }

    /// Set all probabilities from a config struct.
    pub fn set_probabilities(&mut self, config: &FaultInjectionConfig) {
        self.set_probability(FaultType::DiskWrite, config.disk_write_probability);
        self.set_probability(FaultType::DiskRead, config.disk_read_probability);
        self.set_probability(FaultType::DiskFsync, config.disk_fsync_probability);
        self.set_probability(FaultType::NetworkConnectRefused, config.network_connect_probability);
        self.set_probability(FaultType::NetworkBindFailed, config.network_bind_probability);
        self.set_probability(FaultType::NetworkListenFailed, config.network_listen_probability);
        self.set_probability(FaultType::NetworkAcceptFailed, config.network_accept_probability);
        self.set_probability(FaultType::NetworkSendFailed, config.network_send_probability);
        self.set_probability(FaultType::NetworkRecvFailed, config.network_recv_probability);
    }

    /// Schedule a fault to be injected at a specific logical time.
    pub fn schedule_fault(&mut self, fault: ScheduledFault) {
        let time_ns = fault.time_ns;
        self.scheduled_faults.entry(time_ns).or_default().push(fault);
        debug!("Scheduled fault at time_ns={}", time_ns);
    }

    /// Check if a scheduled fault should be triggered.
    fn check_scheduled_fault(&mut self, fault_type: FaultType, current_time_ns: u64, target: &str) -> Option<ScheduledFault> {
        // Find any scheduled faults at or before current time
        let mut to_remove = Vec::new();
        let mut result = None;

        for (&time_ns, faults) in self.scheduled_faults.range_mut(..=current_time_ns) {
            for fault in faults.iter_mut() {
                if fault.consumed {
                    continue;
                }
                if fault.fault_type != fault_type {
                    continue;
                }
                // Check target filter if specified
                if let Some(ref filter) = fault.target {
                    if !target.contains(filter) {
                        continue;
                    }
                }
                fault.consumed = true;
                result = Some(fault.clone());
                break;
            }
            // Mark for cleanup if all consumed
            if faults.iter().all(|f| f.consumed) {
                to_remove.push(time_ns);
            }
            if result.is_some() {
                break;
            }
        }

        // Cleanup consumed entries
        for time_ns in to_remove {
            self.scheduled_faults.remove(&time_ns);
        }

        result
    }

    /// Check if a fault should be injected.
    ///
    /// This is the main entry point for fault checking. It:
    /// 1. Checks if fault injection is enabled
    /// 2. Checks for scheduled faults at the current time
    /// 3. Probabilistically decides based on configured probability
    ///
    /// # Arguments
    /// * `fault_type` - The type of fault to check for.
    /// * `target` - Context string (task name, path, etc.) for logging.
    /// * `current_time_ns` - Current logical time in nanoseconds.
    ///
    /// # Returns
    /// A `FaultCheckResult` indicating whether to inject and details.
    pub fn should_inject_fault(&mut self, fault_type: FaultType, target: &str, current_time_ns: u64) -> FaultCheckResult {
        self.fault_checks_count.fetch_add(1, Ordering::Relaxed);

        if !self.enabled {
            return FaultCheckResult::no_fault();
        }

        // Check scheduled faults first (these are deterministic)
        if let Some(scheduled) = self.check_scheduled_fault(fault_type, current_time_ns, target) {
            self.faults_injected_count.fetch_add(1, Ordering::Relaxed);
            let msg = format!(
                "Scheduled {} fault at time_ns={} target={}",
                fault_type, scheduled.time_ns, target
            );
            warn!("DST: {}", msg);
            return FaultCheckResult::fault(fault_type, msg);
        }

        // Check probabilistic fault
        let probability = self.get_probability(fault_type);
        if probability <= 0.0 {
            return FaultCheckResult::no_fault();
        }

        // Generate deterministic random value
        let roll: f64 = self.rng.r#gen();

        trace!(
            "FaultInjector: {} check roll={:.4} prob={:.4} target={}",
            fault_type, roll, probability, target
        );

        if roll < probability {
            self.faults_injected_count.fetch_add(1, Ordering::Relaxed);
            let msg = format!(
                "Probabilistic {} fault (roll={:.4} < prob={:.4}) target={}",
                fault_type, roll, probability, target
            );
            warn!("DST: {}", msg);
            return FaultCheckResult::fault(fault_type, msg);
        }

        FaultCheckResult::no_fault()
    }

    /// Check if a fault should be injected for a file descriptor operation.
    /// Skips protected FDs (stdin/stdout/stderr).
    pub fn should_inject_fd_fault(&mut self, fault_type: FaultType, fd: i32, target: &str, current_time_ns: u64) -> FaultCheckResult {
        if self.protected_fds.contains(&fd) {
            trace!("FaultInjector: skipping protected fd {}", fd);
            return FaultCheckResult::no_fault();
        }
        self.should_inject_fault(fault_type, target, current_time_ns)
    }

    /// Get statistics about fault injection.
    pub fn get_stats(&self) -> FaultInjectorStats {
        FaultInjectorStats {
            enabled: self.enabled,
            checks_performed: self.fault_checks_count.load(Ordering::Relaxed),
            faults_injected: self.faults_injected_count.load(Ordering::Relaxed),
            scheduled_faults_pending: self.scheduled_faults.values().map(|v| v.len() as u64).sum(),
            probabilities: self.probabilities.clone(),
        }
    }

    /// Reset all state (for testing).
    pub fn reset(&mut self) {
        self.faults_injected_count.store(0, Ordering::Relaxed);
        self.fault_checks_count.store(0, Ordering::Relaxed);
        self.scheduled_faults.clear();
    }
}

/// Statistics from the fault injector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultInjectorStats {
    /// Whether fault injection is enabled.
    pub enabled: bool,
    /// Number of fault checks performed.
    pub checks_performed: u64,
    /// Number of faults actually injected.
    pub faults_injected: u64,
    /// Number of scheduled faults pending.
    pub scheduled_faults_pending: u64,
    /// Current probabilities per fault type.
    pub probabilities: HashMap<FaultType, f64>,
}

/// Configuration for fault injection probabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FaultInjectionConfig {
    /// Probability of disk write failures.
    pub disk_write_probability: f64,
    /// Probability of disk read failures.
    pub disk_read_probability: f64,
    /// Probability of disk fsync failures.
    pub disk_fsync_probability: f64,
    /// Probability of network connect failures.
    pub network_connect_probability: f64,
    /// Probability of network bind failures.
    pub network_bind_probability: f64,
    /// Probability of network listen failures.
    pub network_listen_probability: f64,
    /// Probability of network accept failures.
    pub network_accept_probability: f64,
    /// Probability of network send failures.
    pub network_send_probability: f64,
    /// Probability of network recv failures.
    pub network_recv_probability: f64,
}

impl FaultInjectionConfig {
    /// Check if any fault injection is enabled.
    pub fn has_any_faults(&self) -> bool {
        self.disk_write_probability > 0.0
            || self.disk_read_probability > 0.0
            || self.disk_fsync_probability > 0.0
            || self.network_connect_probability > 0.0
            || self.network_bind_probability > 0.0
            || self.network_listen_probability > 0.0
            || self.network_accept_probability > 0.0
            || self.network_send_probability > 0.0
            || self.network_recv_probability > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fault_injector_deterministic() {
        // Same seed should produce same results
        let mut fi1 = FaultInjector::new(42);
        let mut fi2 = FaultInjector::new(42);

        fi1.set_probability(FaultType::DiskWrite, 0.5);
        fi2.set_probability(FaultType::DiskWrite, 0.5);

        for i in 0..100 {
            let r1 = fi1.should_inject_fault(FaultType::DiskWrite, "test", i);
            let r2 = fi2.should_inject_fault(FaultType::DiskWrite, "test", i);
            assert_eq!(r1.should_fault, r2.should_fault, "Mismatch at iteration {}", i);
        }
    }

    #[test]
    fn test_fault_injector_probability() {
        let mut fi = FaultInjector::new(12345);
        fi.set_probability(FaultType::DiskWrite, 0.5);

        let mut faults = 0;
        let trials = 1000;
        for i in 0..trials {
            if fi.should_inject_fault(FaultType::DiskWrite, "test", i).should_fault {
                faults += 1;
            }
        }

        // With 50% probability, expect roughly 500 faults
        let rate = faults as f64 / trials as f64;
        assert!(rate > 0.4 && rate < 0.6, "Fault rate {} out of expected range", rate);
    }

    #[test]
    fn test_scheduled_fault() {
        let mut fi = FaultInjector::new(42);
        fi.set_enabled(true);

        fi.schedule_fault(ScheduledFault {
            time_ns: 1000,
            fault_type: FaultType::DiskWrite,
            target: None,
            consumed: false,
        });

        // Before scheduled time
        assert!(!fi.should_inject_fault(FaultType::DiskWrite, "test", 500).should_fault);

        // At scheduled time
        assert!(fi.should_inject_fault(FaultType::DiskWrite, "test", 1000).should_fault);

        // After consumed, should not fault again
        assert!(!fi.should_inject_fault(FaultType::DiskWrite, "test", 1001).should_fault);
    }

    #[test]
    fn test_protected_fds() {
        let mut fi = FaultInjector::new(42);
        fi.set_probability(FaultType::DiskWrite, 1.0); // 100% fault rate

        // Protected FDs should not fault
        assert!(!fi.should_inject_fd_fault(FaultType::DiskWrite, 0, "stdin", 0).should_fault);
        assert!(!fi.should_inject_fd_fault(FaultType::DiskWrite, 1, "stdout", 0).should_fault);
        assert!(!fi.should_inject_fd_fault(FaultType::DiskWrite, 2, "stderr", 0).should_fault);

        // Regular FDs should fault
        assert!(fi.should_inject_fd_fault(FaultType::DiskWrite, 3, "file", 0).should_fault);
    }

    #[test]
    fn test_fault_type_errno() {
        assert_eq!(FaultType::DiskWrite.default_errno(), libc::EIO);
        assert_eq!(FaultType::NetworkConnectRefused.default_errno(), libc::ECONNREFUSED);
        assert_eq!(FaultType::NetworkBindFailed.default_errno(), libc::EADDRINUSE);
    }
}
