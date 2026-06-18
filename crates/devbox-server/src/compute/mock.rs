//! Mock compute client for testing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::{
    AsgConfig, AsgDescription, AsgInstance, Compute, LaunchTemplateConfig,
    LaunchTemplateResult, LifecycleHookConfig,
};

/// Internal state of the mock ASG.
struct MockAsgState {
    launch_template: Option<MockLaunchTemplate>,
    asg: Option<MockAsg>,
    instances: HashMap<String, MockInstance>,
}

/// Stored launch template configuration.
struct MockLaunchTemplate {
    id: String,
    version: i64,
    config: LaunchTemplateConfig,
}

/// Stored ASG configuration.
#[allow(dead_code, reason = "fields are written but only read in describe_asg")]
struct MockAsg {
    name: String,
    desired_capacity: u32,
    min_size: u32,
    max_size: u32,
    lifecycle_hook: Option<LifecycleHookConfig>,
    propagate_tags_at_launch: bool,
    pool_id: String,
    managed_by: String,
}

/// A single mock instance in the ASG.
struct MockInstance {
    instance_id: String,
    lifecycle_state: String,
    health_status: String,
    protected_from_scale_in: bool,
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
    /// Create a new mock client with empty state.
    pub fn new() -> Self {
        Self {
            asg_state: Arc::new(Mutex::new(MockAsgState {
                launch_template: None,
                asg: None,
                instances: HashMap::new(),
            })),
            errors: Arc::new(Mutex::new(MockErrors::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Add an instance with the given lifecycle state.
    ///
    /// Returns the generated instance ID (e.g., "i-mock-0001").
    /// If `propagate_tags_at_launch` is true and an ASG exists,
    /// applies pool_id and managed_by tags to the new instance.
    pub fn add_instance(&self, lifecycle_state: &str) -> String {
        let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);
        let instance_id = format!("i-mock-{id_num:04}");

        let mut state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut tags = HashMap::new();

        // If propagate_tags_at_launch is enabled and ASG exists, apply tags.
        if let Some(ref asg) = state.asg
            && asg.propagate_tags_at_launch
        {
            tags.insert("devbox:pool".to_string(), asg.pool_id.clone());
            tags.insert(
                "devbox:managed-by".to_string(),
                asg.managed_by.clone(),
            );
        }

        state.instances.insert(
            instance_id.clone(),
            MockInstance {
                instance_id: instance_id.clone(),
                lifecycle_state: lifecycle_state.to_string(),
                health_status: "Healthy".to_string(),
                protected_from_scale_in: false,
                tags,
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
    ///
    /// The error is consumed (removed) on the next call to that method.
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

    /// Get the current `propagate_tags_at_launch` setting.
    pub fn get_propagate_tags_at_launch(&self) -> bool {
        let state = self
            .asg_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .asg
            .as_ref()
            .is_some_and(|asg| asg.propagate_tags_at_launch)
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
    async fn ensure_launch_template(
        &self,
        config: &LaunchTemplateConfig,
    ) -> Result<LaunchTemplateResult> {
        self.check_error("ensure_launch_template")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let lt_id = "lt-mock".to_string();

        let version = if let Some(ref existing) = state.launch_template {
            // Check if config changed — compare relevant fields.
            let changed = existing.config.ami_id != config.ami_id
                || existing.config.instance_type != config.instance_type
                || existing.config.cpu != config.cpu
                || existing.config.memory_mib != config.memory_mib
                || existing.config.security_group_ids != config.security_group_ids;

            if changed {
                existing
                    .version
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("version overflow"))?
            } else {
                existing.version
            }
        } else {
            1
        };

        state.launch_template = Some(MockLaunchTemplate {
            id: lt_id.clone(),
            version,
            config: config.clone(),
        });

        Ok(LaunchTemplateResult { id: lt_id, version })
    }

    async fn ensure_asg(&self, config: &AsgConfig) -> Result<String> {
        self.check_error("ensure_asg")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        state.asg = Some(MockAsg {
            name: config.name.clone(),
            desired_capacity: config.desired_capacity,
            min_size: config.min_size,
            max_size: config.max_size,
            lifecycle_hook: None,
            propagate_tags_at_launch: config.propagate_tags_at_launch,
            pool_id: config.pool_id.clone(),
            managed_by: config.managed_by.clone(),
        });

        Ok(config.name.clone())
    }

    async fn ensure_lifecycle_hook(&self, config: &LifecycleHookConfig) -> Result<()> {
        self.check_error("ensure_lifecycle_hook")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let asg = state
            .asg
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ASG not created yet"))?;

        asg.lifecycle_hook = Some(config.clone());

        Ok(())
    }

    async fn set_desired_capacity(&self, _asg_name: &str, desired: u32) -> Result<()> {
        self.check_error("set_desired_capacity")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let asg = state
            .asg
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ASG not created yet"))?;

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
            .ok_or_else(|| anyhow::anyhow!("ASG not created yet"))?;

        let lt_id = state
            .launch_template
            .as_ref()
            .map(|lt| lt.id.clone());
        let lt_version = state
            .launch_template
            .as_ref()
            .map(|lt| lt.version.to_string());

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
            launch_template_id: lt_id,
            launch_template_version: lt_version,
            instances,
        })
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
            .ok_or_else(|| {
                anyhow::anyhow!("instance {instance_id} not found")
            })?;

        if should_decrement
            && let Some(ref mut asg) = state.asg
        {
            asg.desired_capacity =
                asg.desired_capacity.saturating_sub(1);
        }

        Ok(())
    }

    async fn complete_lifecycle_action(
        &self,
        _asg_name: &str,
        _hook_name: &str,
        instance_id: &str,
        _action_result: &str,
    ) -> Result<()> {
        self.check_error("complete_lifecycle_action")?;

        let state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        // Validate the instance exists.
        if !state.instances.contains_key(instance_id) {
            return Err(anyhow::anyhow!(
                "instance {instance_id} not found"
            ));
        }

        Ok(())
    }

    async fn tag_instance(
        &self,
        instance_id: &str,
        tags: &[(&str, &str)],
    ) -> Result<()> {
        self.check_error("tag_instance")?;

        let mut state = self
            .asg_state
            .lock()
            .map_err(|e| anyhow::anyhow!("lock poisoned: {e}"))?;

        let instance = state
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| {
                anyhow::anyhow!("instance {instance_id} not found")
            })?;

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
                .ok_or_else(|| {
                    anyhow::anyhow!("instance {id} not found")
                })?;
            instance.protected_from_scale_in = protected;
        }

        Ok(())
    }
}
