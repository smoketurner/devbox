//! Mock compute client for testing the adopt-only reconciler.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::{AsgDescription, AsgInstance, Compute, InstanceInfo};

/// Internal state of the mock ASG (provisioned out-of-band, like Terraform).
struct MockAsgState {
    asg: Option<MockAsg>,
    instances: HashMap<String, MockInstance>,
}

/// Stored ASG state the reconciler adopts and reads.
struct MockAsg {
    desired_capacity: u32,
    min_size: u32,
    max_size: u32,
}

/// A single mock instance in the ASG.
struct MockInstance {
    instance_id: String,
    lifecycle_state: String,
    health_status: String,
    protected_from_scale_in: bool,
    instance_type: String,
    ami_id: String,
    subnet_id: String,
    tags: HashMap<String, String>,
}

/// Error injection storage, keyed by method name.
struct MockErrors {
    errors: HashMap<String, String>,
}

impl MockErrors {
    fn new() -> Self {
        Self {
            errors: HashMap::new(),
        }
    }

    /// Check if an error is set for the given method, consuming it if present.
    fn take(&mut self, method: &str) -> Option<String> {
        self.errors.remove(method)
    }
}

/// Mock compute client for testing the reconciler without real AWS calls.
pub struct MockCompute {
    /// Internal ASG state.
    asg_state: Arc<Mutex<MockAsgState>>,
    /// Error injection.
    errors: Arc<Mutex<MockErrors>>,
    /// Counter for generating deterministic instance IDs.
    next_id: Arc<AtomicU64>,
}

impl MockCompute {
    /// Create a new mock client with no ASG (Terraform has not "applied" yet).
    pub fn new() -> Self {
        Self {
            asg_state: Arc::new(Mutex::new(MockAsgState {
                asg: None,
                instances: HashMap::new(),
            })),
            errors: Arc::new(Mutex::new(MockErrors::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Seed the ASG that the reconciler adopts (stands in for Terraform).
    pub fn seed_asg(&self, min_size: u32, max_size: u32, desired_capacity: u32) {
        let mut state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.asg = Some(MockAsg {
            desired_capacity,
            min_size,
            max_size,
        });
    }

    /// Add an instance with the given lifecycle state, returning its ID.
    pub fn add_instance(&self, lifecycle_state: &str) -> String {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let instance_id = format!("i-mock-{id_num:04}");

        let mut state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        state.instances.insert(
            instance_id.clone(),
            MockInstance {
                instance_id: instance_id.clone(),
                lifecycle_state: lifecycle_state.to_string(),
                health_status: "Healthy".to_string(),
                protected_from_scale_in: false,
                instance_type: "m7g.large".to_string(),
                ami_id: "ami-mock0000000000".to_string(),
                subnet_id: "subnet-mock00000000".to_string(),
                tags: HashMap::new(),
            },
        );

        instance_id
    }

    /// Update the lifecycle state of an existing instance.
    pub fn set_instance_lifecycle_state(&self, id: &str, state: &str) {
        let mut asg_state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(instance) = asg_state.instances.get_mut(id) {
            instance.lifecycle_state = state.to_string();
        }
    }

    /// Inject an error to be returned on the next call to the specified method.
    pub fn set_error(&self, method: &str, error: String) {
        let mut errors = self
            .errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        errors.errors.insert(method.to_string(), error);
    }

    /// Get the tags for a specific instance, for test assertions.
    pub fn get_instance_tags(&self, id: &str) -> Option<HashMap<String, String>> {
        let state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.instances.get(id).map(|inst| inst.tags.clone())
    }

    /// Set or remove the `devbox:ready` tag on an instance.
    ///
    /// Mirrors the real implementation: readiness is stored as a tag, not a
    /// separate bool, so describe_instances derives `InstanceInfo.ready` from it.
    pub fn set_instance_ready(&self, id: &str, ready: bool) {
        let mut state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(instance) = state.instances.get_mut(id) {
            if ready {
                instance
                    .tags
                    .insert("devbox:ready".to_string(), "true".to_string());
            } else {
                instance.tags.remove("devbox:ready");
            }
        }
    }

    /// Check for an injected error and return it if present.
    fn check_error(&self, method: &str) -> Result<()> {
        let mut errors = self
            .errors
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;
        if let Some(msg) = errors.take(method) {
            return Err(anyhow::anyhow!("{msg}"));
        }
        Ok(())
    }
}

impl Default for MockCompute {
    fn default() -> Self {
        Self::new()
    }
}

impl Compute for MockCompute {
    async fn set_desired_capacity(&self, _asg_name: &str, desired: u32) -> Result<()> {
        self.check_error("set_desired_capacity")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let asg = state
            .asg
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ASG not provisioned"))?;

        asg.desired_capacity = desired;

        Ok(())
    }

    async fn describe_asg(&self, _asg_name: &str) -> Result<AsgDescription> {
        self.check_error("describe_asg")?;

        let state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let asg = state
            .asg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ASG not provisioned"))?;

        let instances = state
            .instances
            .values()
            .map(|inst| AsgInstance {
                instance_id: inst.instance_id.clone(),
                lifecycle_state: inst.lifecycle_state.clone(),
                health_status: inst.health_status.clone(),
                protected_from_scale_in: inst.protected_from_scale_in,
            })
            .collect();

        Ok(AsgDescription {
            desired_capacity: asg.desired_capacity,
            min_size: asg.min_size,
            max_size: asg.max_size,
            launch_template_id: None,
            launch_template_version: None,
            instances,
        })
    }

    async fn describe_instances(&self, instance_ids: &[&str]) -> Result<Vec<InstanceInfo>> {
        self.check_error("describe_instances")?;

        let state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let infos = instance_ids
            .iter()
            .filter_map(|id| state.instances.get(*id))
            .map(|inst| InstanceInfo {
                instance_id: inst.instance_id.clone(),
                instance_type: inst.instance_type.clone(),
                ami_id: inst.ami_id.clone(),
                subnet_id: inst.subnet_id.clone(),
                ready: inst.tags.get("devbox:ready").map(String::as_str) == Some("true"),
            })
            .collect();

        Ok(infos)
    }

    async fn terminate_instance_in_asg(
        &self,
        instance_id: &str,
        should_decrement: bool,
    ) -> Result<()> {
        self.check_error("terminate_instance_in_asg")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        state
            .instances
            .remove(instance_id)
            .ok_or_else(|| anyhow::anyhow!("instance {instance_id} not found"))?;

        if should_decrement && let Some(ref mut asg) = state.asg {
            asg.desired_capacity = asg.desired_capacity.saturating_sub(1);
        }

        Ok(())
    }

    async fn tag_instance(&self, instance_id: &str, tags: &[(&str, &str)]) -> Result<()> {
        self.check_error("tag_instance")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let instance = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| anyhow::anyhow!("instance {instance_id} not found"))?;

        for (key, value) in tags {
            instance
                .tags
                .insert((*key).to_string(), (*value).to_string());
        }

        Ok(())
    }

    async fn set_scale_in_protection(
        &self,
        _asg_name: &str,
        instance_ids: &[&str],
        protected: bool,
    ) -> Result<()> {
        self.check_error("set_scale_in_protection")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        for id in instance_ids {
            let instance = state
                .instances
                .get_mut(*id)
                .ok_or_else(|| anyhow::anyhow!("instance {id} not found"))?;
            instance.protected_from_scale_in = protected;
        }

        Ok(())
    }
}
