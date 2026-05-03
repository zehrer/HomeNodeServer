use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use homenode_sdk::is_known_module_id;
use homenode_sdk::proto::home_node_control_server::HomeNodeControl;
use homenode_sdk::proto::{
    DeviceRecord, Empty, ModuleHealth, ModuleRegistration, ModuleRuntime, RuntimeSnapshot,
    UpsertDevicesRequest,
};

#[derive(Debug, Default)]
pub struct RuntimeState {
    modules: BTreeMap<String, ModuleRuntime>,
    devices: BTreeMap<(String, String), DeviceRecord>,
}

pub type SharedState = Arc<RwLock<RuntimeState>>;

impl RuntimeState {
    pub fn register_module(&mut self, registration: ModuleRegistration) -> Result<(), Status> {
        let manifest = registration
            .manifest
            .ok_or_else(|| Status::invalid_argument("missing manifest"))?;
        let health = registration
            .initial_health
            .ok_or_else(|| Status::invalid_argument("missing initial health"))?;

        if manifest.id != health.module_id {
            return Err(Status::invalid_argument(
                "manifest.id and initial_health.module_id must match",
            ));
        }

        if !is_known_module_id(&manifest.id) {
            return Err(Status::invalid_argument("unknown module id"));
        }

        if self.modules.contains_key(&manifest.id) {
            return Err(Status::already_exists("module is already registered"));
        }

        self.modules.insert(
            manifest.id.clone(),
            ModuleRuntime {
                manifest: Some(manifest),
                health: Some(health),
                connected: true,
            },
        );

        Ok(())
    }

    pub fn update_health(&mut self, health: ModuleHealth) -> Result<(), Status> {
        let runtime = self
            .modules
            .get_mut(&health.module_id)
            .ok_or_else(|| Status::not_found("module is not registered"))?;
        runtime.health = Some(health);
        runtime.connected = true;
        Ok(())
    }

    pub fn replace_devices(&mut self, request: UpsertDevicesRequest) -> Result<(), Status> {
        if !self.modules.contains_key(&request.module_id) {
            return Err(Status::not_found("module is not registered"));
        }

        self.devices
            .retain(|(module_id, _), _| module_id != &request.module_id);

        for device in request.devices {
            if device.module_id != request.module_id {
                return Err(Status::invalid_argument(
                    "device.module_id must match request.module_id",
                ));
            }

            self.devices
                .insert((device.module_id.clone(), device.device_id.clone()), device);
        }

        Ok(())
    }

    pub fn mark_disconnected(&mut self, module_id: &str, message: String) {
        if let Some(runtime) = self.modules.get_mut(module_id) {
            runtime.connected = false;
            if let Some(health) = runtime.health.as_mut() {
                health.message = message;
                health.updated_at = homenode_sdk::now_timestamp_secs();
            }
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            modules: self.modules.values().cloned().collect(),
            devices: self.devices.values().cloned().collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlService {
    state: SharedState,
}

impl ControlService {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl HomeNodeControl for ControlService {
    async fn register_module(
        &self,
        request: Request<ModuleRegistration>,
    ) -> Result<Response<Empty>, Status> {
        self.state
            .write()
            .await
            .register_module(request.into_inner())?;
        Ok(Response::new(Empty {}))
    }

    async fn report_health(
        &self,
        request: Request<ModuleHealth>,
    ) -> Result<Response<Empty>, Status> {
        self.state.write().await.update_health(request.into_inner())?;
        Ok(Response::new(Empty {}))
    }

    async fn upsert_devices(
        &self,
        request: Request<UpsertDevicesRequest>,
    ) -> Result<Response<Empty>, Status> {
        self.state
            .write()
            .await
            .replace_devices(request.into_inner())?;
        Ok(Response::new(Empty {}))
    }

    async fn get_runtime_snapshot(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<RuntimeSnapshot>, Status> {
        let snapshot = self.state.read().await.snapshot();
        Ok(Response::new(snapshot))
    }
}
