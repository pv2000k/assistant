use std::{
    collections::HashMap,
    env,
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LEASE_TIMEOUT_SECS: u64 = 15;

pub struct RuntimeLifecycle {
    clients: Mutex<HashMap<String, Instant>>,
    active_requests: AtomicUsize,
    ever_had_client: AtomicBool,
    idle_since: Mutex<Option<Instant>>,
    idle_timeout: Duration,
    lease_timeout: Duration,
}

pub struct RequestGuard {
    lifecycle: Arc<RuntimeLifecycle>,
}

impl RuntimeLifecycle {
    pub fn from_environment() -> Result<Self, Box<dyn Error>> {
        let idle_timeout =
            duration_from_environment("ASSISTANT_IDLE_TIMEOUT_SECS", DEFAULT_IDLE_TIMEOUT_SECS)?;
        let lease_timeout =
            duration_from_environment("ASSISTANT_LEASE_TIMEOUT_SECS", DEFAULT_LEASE_TIMEOUT_SECS)?;

        Ok(Self::new(idle_timeout, lease_timeout))
    }

    pub fn new(idle_timeout: Duration, lease_timeout: Duration) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            active_requests: AtomicUsize::new(0),
            ever_had_client: AtomicBool::new(false),
            idle_since: Mutex::new(None),
            idle_timeout,
            lease_timeout,
        }
    }

    pub fn begin_request(self: &Arc<Self>) -> RequestGuard {
        self.active_requests.fetch_add(1, Ordering::AcqRel);
        RequestGuard {
            lifecycle: Arc::clone(self),
        }
    }

    pub fn acquire(&self, client_id: &str) -> Result<usize, String> {
        validate_client_id(client_id)?;
        let now = Instant::now();
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "Runtime client lease state lock was poisoned.".to_string())?;
        clients.insert(client_id.to_string(), now);
        self.ever_had_client.store(true, Ordering::Release);
        let active_clients = clients.len();
        drop(clients);

        let mut idle_since = self
            .idle_since
            .lock()
            .map_err(|_| "Runtime idle state lock was poisoned.".to_string())?;
        *idle_since = None;

        Ok(active_clients)
    }

    pub fn heartbeat(&self, client_id: &str) -> Result<usize, String> {
        validate_client_id(client_id)?;
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "Runtime client lease state lock was poisoned.".to_string())?;
        let lease = clients
            .get_mut(client_id)
            .ok_or_else(|| "Unknown or expired client lease.".to_string())?;
        *lease = Instant::now();
        Ok(clients.len())
    }

    pub fn release(&self, client_id: &str) -> Result<usize, String> {
        validate_client_id(client_id)?;
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "Runtime client lease state lock was poisoned.".to_string())?;
        if clients.remove(client_id).is_none() {
            return Err("Unknown or expired client lease.".to_string());
        }
        Ok(clients.len())
    }

    pub fn should_shutdown(&self, background_work_active: bool) -> bool {
        let now = Instant::now();

        let has_clients = match self.clients.lock() {
            Ok(mut clients) => {
                clients.retain(|_, last_seen| now.duration_since(*last_seen) <= self.lease_timeout);
                !clients.is_empty()
            }
            Err(_) => return false,
        };

        let active_requests = self.active_requests.load(Ordering::Acquire);
        if has_clients || active_requests != 0 || background_work_active {
            if let Ok(mut idle_since) = self.idle_since.lock() {
                *idle_since = None;
            }
            return false;
        }

        if !self.ever_had_client.load(Ordering::Acquire) {
            return false;
        }

        let mut idle_since = match self.idle_since.lock() {
            Ok(idle_since) => idle_since,
            Err(_) => return false,
        };
        let started = idle_since.get_or_insert(now);
        now.duration_since(*started) >= self.idle_timeout
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.lifecycle
            .active_requests
            .fetch_sub(1, Ordering::AcqRel);
    }
}

fn duration_from_environment(name: &str, default_seconds: u64) -> Result<Duration, Box<dyn Error>> {
    let value = env::var(name).unwrap_or_else(|_| default_seconds.to_string());
    let seconds = value
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("{name} must be a non-negative integer: {error}"))?;
    Ok(Duration::from_secs(seconds))
}

fn validate_client_id(client_id: &str) -> Result<(), String> {
    let client_id = client_id.trim();
    if client_id.is_empty() || client_id.chars().count() > 128 || client_id.contains(['\n', '\r']) {
        return Err(
            "Client id must be non-empty, single-line, and at most 128 characters.".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle() -> RuntimeLifecycle {
        RuntimeLifecycle::new(Duration::ZERO, Duration::from_secs(60))
    }

    #[test]
    fn lease_prevents_shutdown_until_released() {
        let lifecycle = lifecycle();
        lifecycle.acquire("client-1").expect("lease acquired");
        assert!(!lifecycle.should_shutdown(false));
        lifecycle.release("client-1").expect("lease released");
        assert!(lifecycle.should_shutdown(false));
    }

    #[test]
    fn active_request_delays_shutdown_after_release() {
        let lifecycle = Arc::new(lifecycle());
        lifecycle.acquire("client-1").expect("lease acquired");
        let request = lifecycle.begin_request();
        lifecycle.release("client-1").expect("lease released");
        assert!(!lifecycle.should_shutdown(false));
        drop(request);
        assert!(lifecycle.should_shutdown(false));
    }

    #[test]
    fn background_work_delays_shutdown() {
        let lifecycle = lifecycle();
        lifecycle.acquire("client-1").expect("lease acquired");
        lifecycle.release("client-1").expect("lease released");
        assert!(!lifecycle.should_shutdown(true));
        assert!(lifecycle.should_shutdown(false));
    }

    #[test]
    fn heartbeat_keeps_lease_alive() {
        let lifecycle = RuntimeLifecycle::new(Duration::ZERO, Duration::from_secs(60));
        lifecycle.acquire("client-1").expect("lease acquired");
        lifecycle.heartbeat("client-1").expect("heartbeat succeeds");
        assert!(!lifecycle.should_shutdown(false));
    }

    #[test]
    fn unknown_release_and_heartbeat_are_rejected() {
        let lifecycle = lifecycle();
        assert!(lifecycle.release("unknown").is_err());
        assert!(lifecycle.heartbeat("unknown").is_err());
    }

    #[test]
    fn second_client_keeps_runtime_alive_after_first_release() {
        let lifecycle = lifecycle();
        lifecycle.acquire("client-1").expect("first lease acquired");
        lifecycle
            .acquire("client-2")
            .expect("second lease acquired");
        lifecycle.release("client-1").expect("first lease released");
        assert!(!lifecycle.should_shutdown(false));
        lifecycle
            .release("client-2")
            .expect("second lease released");
        assert!(lifecycle.should_shutdown(false));
    }
}
