/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * Copyright (c) 2026 Bloodhound Authors.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Control protocol for external coordination.
//!
//! This module provides a Unix socket-based control interface that allows
//! external tools (like Bloodhound) to inspect and control Hermit execution.
//!
//! # Protocol
//!
//! The protocol is JSON-RPC style with newline-delimited messages:
//! - Client sends: `{"method": "GetTime"}\n`
//! - Server responds: `{"success": true, "data": {"time_ns": 12345}}\n`

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::scheduler::Scheduler;
use crate::types::GlobalTime;

use crate::fault_injection::{FaultType, FaultInjectorStats, ScheduledFault, FaultInjector};

/// Commands that can be sent to the control server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum ControlCommand {
    /// Get current virtual time
    GetTime,

    /// Get execution statistics
    GetStats,

    /// Get scheduler state (thread list, run queue)
    GetSchedulerState,

    /// Pause execution (blocks until pause takes effect)
    Pause,

    /// Resume execution
    Resume,

    /// Check if execution is paused
    IsPaused,

    /// Get current seed
    GetSeed,

    /// Graceful shutdown
    Shutdown,

    /// Ping (for connection testing)
    Ping,

    // ==================== DST Fault Injection Commands ====================

    /// Set fault probability for a specific type
    SetFaultProbability {
        /// The fault type (e.g., "disk_write", "network_connect")
        fault_type: String,
        /// Probability between 0.0 and 1.0
        probability: f64,
    },

    /// Schedule a fault at a specific logical time
    ScheduleFault {
        /// Logical time in nanoseconds
        time_ns: u64,
        /// The fault type
        fault_type: String,
        /// Optional target filter
        target: Option<String>,
    },

    /// Get fault injection statistics
    GetFaultStats,

    /// Enable or disable fault injection
    SetFaultInjectionEnabled {
        /// Whether to enable
        enabled: bool,
    },
}

/// Response from control server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    /// Whether the command succeeded
    pub success: bool,
    /// Response data (command-specific)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Error message if failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    /// Create a success response with no data
    pub fn ok() -> Self {
        Self {
            success: true,
            data: None,
            error: None,
        }
    }

    /// Create a success response with data
    pub fn ok_with_data(data: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    /// Create an error response
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// Execution statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStats {
    /// Total syscalls intercepted
    pub syscalls_intercepted: u64,
    /// Total thread switches
    pub thread_switches: u64,
    /// Current virtual time (nanoseconds)
    pub time_ns: u64,
    /// Number of active threads
    pub active_threads: u32,
    /// Number of blocked threads
    pub blocked_threads: u32,
    /// Total RCB quanta executed
    pub quanta_executed: u64,
}

/// Scheduler state snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerSnapshot {
    /// Currently running thread ID (if any)
    pub current_thread: Option<i32>,
    /// Thread IDs in run queue (ordered by priority)
    pub run_queue: Vec<i32>,
    /// Thread IDs blocked on I/O or futex
    pub blocked_threads: Vec<i32>,
    /// Whether scheduler is paused
    pub is_paused: bool,
}

/// Control server state
pub struct ControlServer {
    /// Socket path
    socket_path: PathBuf,
    /// Reference to scheduler for state queries
    scheduler: Arc<Mutex<Scheduler>>,
    /// Reference to global time
    global_time: Arc<Mutex<GlobalTime>>,
    /// Seed used for this run
    seed: u64,
    /// Pause flag
    paused: Arc<AtomicBool>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Stats counters
    stats: Arc<Mutex<ControlStats>>,
    /// Fault injector for DST
    fault_injector: Option<Arc<Mutex<FaultInjector>>>,
}

/// Internal stats tracking
#[derive(Debug, Default)]
struct ControlStats {
    syscalls_intercepted: u64,
    thread_switches: u64,
    quanta_executed: u64,
}

impl ControlServer {
    /// Create a new control server
    pub fn new(
        socket_path: impl Into<PathBuf>,
        scheduler: Arc<Mutex<Scheduler>>,
        global_time: Arc<Mutex<GlobalTime>>,
        seed: u64,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            scheduler,
            global_time,
            seed,
            paused: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(ControlStats::default())),
            fault_injector: None,
        }
    }

    /// Create a new control server with fault injector
    pub fn new_with_fault_injector(
        socket_path: impl Into<PathBuf>,
        scheduler: Arc<Mutex<Scheduler>>,
        global_time: Arc<Mutex<GlobalTime>>,
        seed: u64,
        fault_injector: Arc<Mutex<FaultInjector>>,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            scheduler,
            global_time,
            seed,
            paused: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(ControlStats::default())),
            fault_injector: Some(fault_injector),
        }
    }

    /// Get pause flag (for scheduler to check)
    pub fn paused(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }

    /// Get shutdown flag
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// Increment syscall counter
    pub fn record_syscall(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.syscalls_intercepted += 1;
        }
    }

    /// Increment thread switch counter
    pub fn record_thread_switch(&self) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.thread_switches += 1;
        }
    }

    /// Increment quanta counter
    pub fn record_quanta(&self, count: u64) {
        if let Ok(mut stats) = self.stats.lock() {
            stats.quanta_executed += count;
        }
    }

    /// Run the control server (blocking)
    pub fn run(&self) -> std::io::Result<()> {
        // Remove existing socket file if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Control server listening on {:?}", self.socket_path);

        // Set non-blocking for graceful shutdown
        listener.set_nonblocking(true)?;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                info!("Control server shutting down");
                break;
            }

            match listener.accept() {
                Ok((stream, _addr)) => {
                    debug!("Control client connected");
                    if let Err(e) = self.handle_client(stream) {
                        warn!("Error handling control client: {}", e);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No connection, sleep briefly and retry
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                }
            }
        }

        // Cleanup
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Handle a single client connection
    fn handle_client(&self, stream: UnixStream) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    debug!("Control client disconnected");
                    break;
                }
                Ok(_) => {
                    let response = self.handle_command(&line);
                    let mut response_json = serde_json::to_string(&response)?;
                    response_json.push('\n');
                    writer.write_all(response_json.as_bytes())?;
                    writer.flush()?;

                    // Check if shutdown was requested
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error reading from control client: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Handle a single command
    fn handle_command(&self, line: &str) -> ControlResponse {
        let command: ControlCommand = match serde_json::from_str(line.trim()) {
            Ok(cmd) => cmd,
            Err(e) => {
                return ControlResponse::err(format!("Invalid command: {}", e));
            }
        };

        debug!("Control command: {:?}", command);

        match command {
            ControlCommand::Ping => ControlResponse::ok_with_data(serde_json::json!({"pong": true})),

            ControlCommand::GetTime => {
                let time_ns = self.get_current_time_ns();
                ControlResponse::ok_with_data(serde_json::json!({"time_ns": time_ns}))
            }

            ControlCommand::GetStats => {
                let stats = self.get_stats();
                match serde_json::to_value(stats) {
                    Ok(v) => ControlResponse::ok_with_data(v),
                    Err(e) => ControlResponse::err(format!("Failed to serialize stats: {}", e)),
                }
            }

            ControlCommand::GetSchedulerState => {
                let state = self.get_scheduler_state();
                match serde_json::to_value(state) {
                    Ok(v) => ControlResponse::ok_with_data(v),
                    Err(e) => ControlResponse::err(format!("Failed to serialize state: {}", e)),
                }
            }

            ControlCommand::Pause => {
                self.paused.store(true, Ordering::SeqCst);
                info!("Execution paused via control");
                ControlResponse::ok()
            }

            ControlCommand::Resume => {
                self.paused.store(false, Ordering::SeqCst);
                info!("Execution resumed via control");
                ControlResponse::ok()
            }

            ControlCommand::IsPaused => {
                let is_paused = self.paused.load(Ordering::SeqCst);
                ControlResponse::ok_with_data(serde_json::json!({"paused": is_paused}))
            }

            ControlCommand::GetSeed => {
                ControlResponse::ok_with_data(serde_json::json!({"seed": self.seed}))
            }

            ControlCommand::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                info!("Shutdown requested via control");
                ControlResponse::ok()
            }

            // ==================== DST Fault Injection Commands ====================

            ControlCommand::SetFaultProbability { fault_type, probability } => {
                let fi = match &self.fault_injector {
                    Some(fi) => fi,
                    None => return ControlResponse::err("Fault injector not available"),
                };

                let ft = match fault_type.as_str() {
                    "disk_write" => FaultType::DiskWrite,
                    "disk_read" => FaultType::DiskRead,
                    "disk_fsync" => FaultType::DiskFsync,
                    "network_connect_refused" | "network_connect" => FaultType::NetworkConnectRefused,
                    "network_connect_timeout" => FaultType::NetworkConnectTimeout,
                    "network_bind_failed" | "network_bind" => FaultType::NetworkBindFailed,
                    "network_listen_failed" | "network_listen" => FaultType::NetworkListenFailed,
                    "network_accept_failed" | "network_accept" => FaultType::NetworkAcceptFailed,
                    "network_send_failed" | "network_send" => FaultType::NetworkSendFailed,
                    "network_recv_failed" | "network_recv" => FaultType::NetworkRecvFailed,
                    "memory_alloc" => FaultType::MemoryAlloc,
                    "fork_failed" => FaultType::ForkFailed,
                    _ => return ControlResponse::err(format!("Unknown fault type: {}", fault_type)),
                };

                if let Ok(mut injector) = fi.lock() {
                    injector.set_probability(ft, probability);
                    info!("Set {} probability to {}", fault_type, probability);
                    ControlResponse::ok()
                } else {
                    ControlResponse::err("Failed to lock fault injector")
                }
            }

            ControlCommand::ScheduleFault { time_ns, fault_type, target } => {
                let fi = match &self.fault_injector {
                    Some(fi) => fi,
                    None => return ControlResponse::err("Fault injector not available"),
                };

                let ft = match fault_type.as_str() {
                    "disk_write" => FaultType::DiskWrite,
                    "disk_read" => FaultType::DiskRead,
                    "disk_fsync" => FaultType::DiskFsync,
                    "network_connect_refused" | "network_connect" => FaultType::NetworkConnectRefused,
                    "network_connect_timeout" => FaultType::NetworkConnectTimeout,
                    "network_bind_failed" | "network_bind" => FaultType::NetworkBindFailed,
                    "network_listen_failed" | "network_listen" => FaultType::NetworkListenFailed,
                    "network_accept_failed" | "network_accept" => FaultType::NetworkAcceptFailed,
                    "network_send_failed" | "network_send" => FaultType::NetworkSendFailed,
                    "network_recv_failed" | "network_recv" => FaultType::NetworkRecvFailed,
                    "memory_alloc" => FaultType::MemoryAlloc,
                    "fork_failed" => FaultType::ForkFailed,
                    _ => return ControlResponse::err(format!("Unknown fault type: {}", fault_type)),
                };

                if let Ok(mut injector) = fi.lock() {
                    injector.schedule_fault(ScheduledFault {
                        time_ns,
                        fault_type: ft,
                        target,
                        consumed: false,
                    });
                    info!("Scheduled {} fault at time_ns={}", fault_type, time_ns);
                    ControlResponse::ok()
                } else {
                    ControlResponse::err("Failed to lock fault injector")
                }
            }

            ControlCommand::GetFaultStats => {
                let fi = match &self.fault_injector {
                    Some(fi) => fi,
                    None => return ControlResponse::err("Fault injector not available"),
                };

                if let Ok(injector) = fi.lock() {
                    let stats = injector.get_stats();
                    match serde_json::to_value(stats) {
                        Ok(v) => ControlResponse::ok_with_data(v),
                        Err(e) => ControlResponse::err(format!("Failed to serialize stats: {}", e)),
                    }
                } else {
                    ControlResponse::err("Failed to lock fault injector")
                }
            }

            ControlCommand::SetFaultInjectionEnabled { enabled } => {
                let fi = match &self.fault_injector {
                    Some(fi) => fi,
                    None => return ControlResponse::err("Fault injector not available"),
                };

                if let Ok(mut injector) = fi.lock() {
                    injector.set_enabled(enabled);
                    info!("Fault injection enabled: {}", enabled);
                    ControlResponse::ok()
                } else {
                    ControlResponse::err("Failed to lock fault injector")
                }
            }
        }
    }

    /// Get current virtual time in nanoseconds
    fn get_current_time_ns(&self) -> u64 {
        if let Ok(gt) = self.global_time.lock() {
            // GlobalTime::as_nanos() returns LogicalTime
            // LogicalTime::as_nanos() returns u64
            gt.as_nanos().as_nanos()
        } else {
            0
        }
    }

    /// Get execution statistics
    fn get_stats(&self) -> ExecutionStats {
        let (active, blocked) = if let Ok(sched) = self.scheduler.lock() {
            (sched.active_thread_count() as u32, sched.blocked_thread_count() as u32)
        } else {
            (0, 0)
        };

        let stats = self.stats.lock().map(|s| (s.syscalls_intercepted, s.thread_switches, s.quanta_executed))
            .unwrap_or((0, 0, 0));

        ExecutionStats {
            syscalls_intercepted: stats.0,
            thread_switches: stats.1,
            time_ns: self.get_current_time_ns(),
            active_threads: active,
            blocked_threads: blocked,
            quanta_executed: stats.2,
        }
    }

    /// Get scheduler state snapshot
    fn get_scheduler_state(&self) -> SchedulerSnapshot {
        if let Ok(sched) = self.scheduler.lock() {
            SchedulerSnapshot {
                current_thread: sched.current_thread().map(|t| t.as_raw()),
                run_queue: sched.run_queue_snapshot().iter().map(|t| t.as_raw()).collect(),
                blocked_threads: sched.blocked_threads_snapshot().iter().map(|t| t.as_raw()).collect(),
                is_paused: self.paused.load(Ordering::SeqCst),
            }
        } else {
            SchedulerSnapshot {
                current_thread: None,
                run_queue: vec![],
                blocked_threads: vec![],
                is_paused: self.paused.load(Ordering::SeqCst),
            }
        }
    }
}

/// Spawn control server in a background thread
pub fn spawn_control_server(
    socket_path: impl Into<PathBuf>,
    scheduler: Arc<Mutex<Scheduler>>,
    global_time: Arc<Mutex<GlobalTime>>,
    seed: u64,
) -> (std::thread::JoinHandle<()>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let server = ControlServer::new(socket_path, scheduler, global_time, seed);
    let paused = server.paused();
    let shutdown = server.shutdown_flag();

    let handle = std::thread::spawn(move || {
        if let Err(e) = server.run() {
            error!("Control server error: {}", e);
        }
    });

    (handle, paused, shutdown)
}

/// Spawn control server with fault injector in a background thread
pub fn spawn_control_server_with_fault_injector(
    socket_path: impl Into<PathBuf>,
    scheduler: Arc<Mutex<Scheduler>>,
    global_time: Arc<Mutex<GlobalTime>>,
    seed: u64,
    fault_injector: Arc<Mutex<FaultInjector>>,
) -> (std::thread::JoinHandle<()>, Arc<AtomicBool>, Arc<AtomicBool>) {
    let server = ControlServer::new_with_fault_injector(socket_path, scheduler, global_time, seed, fault_injector);
    let paused = server.paused();
    let shutdown = server.shutdown_flag();

    let handle = std::thread::spawn(move || {
        if let Err(e) = server.run() {
            error!("Control server error: {}", e);
        }
    });

    (handle, paused, shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_serialization() {
        let cmd = ControlCommand::GetTime;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("GetTime"));
    }

    #[test]
    fn test_response_ok() {
        let resp = ControlResponse::ok();
        assert!(resp.success);
        assert!(resp.data.is_none());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_response_with_data() {
        let resp = ControlResponse::ok_with_data(serde_json::json!({"time_ns": 1000}));
        assert!(resp.success);
        assert!(resp.data.is_some());
    }

    #[test]
    fn test_response_error() {
        let resp = ControlResponse::err("test error");
        assert!(!resp.success);
        assert_eq!(resp.error, Some("test error".to_string()));
    }
}
