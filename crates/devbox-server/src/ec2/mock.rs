//! Mock EC2 client for testing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use tokio::sync::Mutex;

use super::{Ec2Client, InstanceStatus};

/// Internal state for a mock instance.
struct MockInstance {
    state: InstanceStatus,
    describe_calls: u32,
}

/// Configurable error injection.
#[derive(Default)]
pub struct MockErrors {
    pub launch_error: Option<String>,
    pub terminate_error: Option<String>,
    pub describe_error: Option<String>,
}

/// Mock EC2 client for testing the reconciler without real AWS calls.
pub struct MockEc2Client {
    instances: Arc<Mutex<HashMap<String, MockInstance>>>,
    calls_to_running: u32,
    errors: Arc<Mutex<MockErrors>>,
    next_id: Arc<AtomicU64>,
}

impl MockEc2Client {
    /// Create a new mock client.
    /// `calls_to_running` controls how many describe calls before pending → running.
    pub fn new(calls_to_running: u32) -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
            calls_to_running,
            errors: Arc::new(Mutex::new(MockErrors::default())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Inject an error for the next launch call.
    pub async fn set_launch_error(&self, msg: impl Into<String>) {
        self.errors.lock().await.launch_error = Some(msg.into());
    }

    /// Inject an error for the next terminate call.
    pub async fn set_terminate_error(&self, msg: impl Into<String>) {
        self.errors.lock().await.terminate_error = Some(msg.into());
    }

    /// Inject an error for the next describe call.
    pub async fn set_describe_error(&self, msg: impl Into<String>) {
        self.errors.lock().await.describe_error = Some(msg.into());
    }

    /// Clear all injected errors.
    pub async fn clear_errors(&self) {
        let mut errors = self.errors.lock().await;
        *errors = MockErrors::default();
    }

    /// Get the current number of tracked instances.
    pub async fn instance_count(&self) -> usize {
        self.instances.lock().await.len()
    }
}

impl Ec2Client for MockEc2Client {
    async fn launch_instance(
        &self,
        _instance_type: &str,
        _ami_id: &str,
        _subnet_id: &str,
    ) -> Result<String> {
        // Check for injected error
        let mut errors = self.errors.lock().await;
        if let Some(msg) = errors.launch_error.take() {
            return Err(anyhow!(msg));
        }
        drop(errors);

        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let instance_id = format!("i-mock{id_num:016x}");

        let mut instances = self.instances.lock().await;
        instances.insert(
            instance_id.clone(),
            MockInstance {
                state: InstanceStatus::Pending,
                describe_calls: 0,
            },
        );

        Ok(instance_id)
    }

    async fn terminate_instance(&self, instance_id: &str) -> Result<()> {
        let mut errors = self.errors.lock().await;
        if let Some(msg) = errors.terminate_error.take() {
            return Err(anyhow!(msg));
        }
        drop(errors);

        let mut instances = self.instances.lock().await;
        instances.remove(instance_id);
        Ok(())
    }

    async fn describe_instance(&self, instance_id: &str) -> Result<InstanceStatus> {
        let mut errors = self.errors.lock().await;
        if let Some(msg) = errors.describe_error.take() {
            return Err(anyhow!(msg));
        }
        drop(errors);

        let mut instances = self.instances.lock().await;
        let instance = instances
            .get_mut(instance_id)
            .ok_or_else(|| anyhow!("instance '{instance_id}' not found"))?;

        instance.describe_calls = instance.describe_calls.saturating_add(1);

        if instance.state == InstanceStatus::Pending
            && instance.describe_calls >= self.calls_to_running
        {
            instance.state = InstanceStatus::Running;
        }

        Ok(instance.state.clone())
    }
}
