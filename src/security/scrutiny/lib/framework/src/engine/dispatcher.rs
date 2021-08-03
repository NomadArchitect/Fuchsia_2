// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::model::controller::{ConnectionMode, DataController, HintDataType},
    crate::model::model::DataModel,
    anyhow::{Error, Result},
    serde_json::value::Value,
    std::collections::HashMap,
    std::sync::{Arc, RwLock},
    thiserror::Error,
    uuid::Uuid,
};

#[derive(Error, Debug)]
pub enum DispatcherError {
    #[error("namespace: {0} is already in use and cannot be bound")]
    NamespaceInUse(String),
    #[error("namespace: {0} does not exist, query failing.")]
    NamespaceDoesNotExist(String),
    #[error("namespace: {0} does not support this connection mode.")]
    ConnectionModeDenied(String),
}

/// `ControllerInstance` holds all the additional book-keeping information
/// required to attribute `instance` ownership to a particular controller.
struct ControllerInstance {
    pub instance_id: Uuid,
    pub controller: Arc<dyn DataController>,
}

/// The ControllerDispatcher provides a 1:1 mapping between namespaces and
/// unique DataController instances.
pub struct ControllerDispatcher {
    model: Arc<DataModel>,
    controllers: RwLock<HashMap<String, ControllerInstance>>,
}

impl ControllerDispatcher {
    pub fn new(model: Arc<DataModel>) -> Self {
        Self { model: model, controllers: RwLock::new(HashMap::new()) }
    }

    /// Adding a control will fail if there is a namespace collision. A
    /// namespace should reflect the REST API url e.g "components/manifests"
    pub fn add(
        &mut self,
        instance_id: Uuid,
        namespace: String,
        controller: Arc<dyn DataController>,
    ) -> Result<()> {
        let mut controllers = self.controllers.write().unwrap();
        if controllers.contains_key(&namespace) {
            return Err(Error::new(DispatcherError::NamespaceInUse(namespace)));
        }
        controllers.insert(namespace, ControllerInstance { instance_id, controller });
        Ok(())
    }

    /// Removes all `ControllerInstance` objects with a matching instance_id.
    /// This effectively unhooks all the plugins controllers.
    pub fn remove(&mut self, instance_id: Uuid) {
        let mut controllers = self.controllers.write().unwrap();
        controllers.retain(|_, inst| inst.instance_id != instance_id);
    }

    /// Attempts to service the query if the namespace has a mapping.
    pub fn query(
        &self,
        connection_mode: ConnectionMode,
        namespace: String,
        query: Value,
    ) -> Result<Value> {
        let controllers = self.controllers.read().unwrap();
        if let Some(instance) = controllers.get(&namespace) {
            // Only allow this controller query to go through if the ConnectionMode is greater or
            // equal to the query.
            if instance.controller.connection_mode() >= connection_mode {
                instance.controller.query(Arc::clone(&self.model), query)
            } else {
                Err(Error::new(DispatcherError::ConnectionModeDenied(namespace)))
            }
        } else {
            Err(Error::new(DispatcherError::NamespaceDoesNotExist(namespace)))
        }
    }

    /// Attempts to return the controller description if it has a namespace mapping.
    pub fn description(&self, namespace: String) -> Result<String> {
        let controllers = self.controllers.read().unwrap();
        if let Some(instance) = controllers.get(&namespace) {
            Ok(instance.controller.description())
        } else {
            Err(Error::new(DispatcherError::NamespaceDoesNotExist(namespace)))
        }
    }

    /// Attempts to return the controller usage if it has a namespace mapping.
    pub fn usage(&self, namespace: String) -> Result<String> {
        let controllers = self.controllers.read().unwrap();
        if let Some(instance) = controllers.get(&namespace) {
            Ok(instance.controller.usage())
        } else {
            Err(Error::new(DispatcherError::NamespaceDoesNotExist(namespace)))
        }
    }

    /// Attempts to return the hints for this particular controller.
    pub fn hints(&self, namespace: String) -> Result<Vec<(String, HintDataType)>> {
        let controllers = self.controllers.read().unwrap();
        if let Some(instance) = controllers.get(&namespace) {
            Ok(instance.controller.hints())
        } else {
            Err(Error::new(DispatcherError::NamespaceDoesNotExist(namespace)))
        }
    }

    /// Returns all of the controller namespaces.
    pub fn controllers_all(&self) -> Vec<String> {
        let controllers = self.controllers.read().unwrap();
        let mut hooks = Vec::new();
        for (hook, _controller) in controllers.iter() {
            hooks.push(hook.clone());
        }
        hooks.sort();
        hooks
    }

    /// Returns a list of all controllers associated with a given instance_id.
    pub fn controllers(&self, instance_id: Uuid) -> Vec<String> {
        let controllers = self.controllers.read().unwrap();
        let mut hooks = Vec::new();
        for (hook, controller) in controllers.iter() {
            if controller.instance_id == instance_id {
                hooks.push(hook.clone());
            }
        }
        hooks
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::model::controller::DataController, crate::model::model::ModelEnvironment,
        serde_json::json, tempfile::tempdir,
    };

    struct FakeController {
        pub result: String,
        pub mode: ConnectionMode,
    }

    impl FakeController {
        pub fn new(result: impl Into<String>) -> Self {
            Self { result: result.into(), mode: ConnectionMode::Remote }
        }

        pub fn new_local(result: impl Into<String>) -> Self {
            Self { result: result.into(), mode: ConnectionMode::Local }
        }
    }

    impl DataController for FakeController {
        fn query(&self, _: Arc<DataModel>, _: Value) -> Result<Value> {
            Ok(json!(self.result))
        }

        fn description(&self) -> String {
            "foo".to_string()
        }

        fn usage(&self) -> String {
            "bar".to_string()
        }

        fn connection_mode(&self) -> ConnectionMode {
            self.mode.clone()
        }

        fn hints(&self) -> Vec<(String, HintDataType)> {
            vec![("foo".to_string(), HintDataType::NoType)]
        }
    }

    fn test_model() -> Arc<DataModel> {
        let store_dir = tempdir().unwrap();
        let build_tmp_dir = tempdir().unwrap();
        let repository_tmp_dir = tempdir().unwrap();
        let uri = store_dir.into_path().into_os_string().into_string().unwrap();
        let build_path = build_tmp_dir.into_path();
        let repository_path = repository_tmp_dir.into_path();
        Arc::new(DataModel::connect(ModelEnvironment { uri, build_path, repository_path }).unwrap())
    }

    #[test]
    fn test_query() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let namespace = "/foo/bar".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        assert_eq!(
            dispatcher.query(ConnectionMode::Remote, namespace, json!("")).unwrap(),
            json!("fake_result")
        );
    }

    #[test]
    fn test_query_removed() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let namespace = "/foo/bar".to_string();
        let inst_id = Uuid::new_v4();
        dispatcher.add(inst_id.clone(), namespace.clone(), fake).unwrap();
        assert_eq!(
            dispatcher.query(ConnectionMode::Remote, namespace.clone(), json!("")).unwrap(),
            json!("fake_result")
        );
        dispatcher.remove(inst_id);
        assert!(dispatcher.query(ConnectionMode::Remote, namespace, json!("")).is_err());
    }

    #[test]
    fn test_query_multiple() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let fake_two = Arc::new(FakeController::new("fake_result_two"));
        let namespace = "/foo/bar".to_string();
        let namespace_two = "/foo/baz".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        dispatcher.add(Uuid::new_v4(), namespace_two.clone(), fake_two).unwrap();
        assert_eq!(
            dispatcher.query(ConnectionMode::Remote, namespace, json!("")).unwrap(),
            json!("fake_result")
        );
        assert_eq!(
            dispatcher.query(ConnectionMode::Remote, namespace_two, json!("")).unwrap(),
            json!("fake_result_two")
        );
    }

    #[test]
    fn test_description() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let namespace = "/foo/bar".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        assert_eq!(dispatcher.description(namespace).unwrap(), "foo");
    }

    #[test]
    fn test_usage() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let namespace = "/foo/bar".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        assert_eq!(dispatcher.usage(namespace).unwrap(), "bar");
    }

    #[test]
    fn test_hints() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new("fake_result"));
        let namespace = "/foo/bar".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        assert_eq!(dispatcher.hints(namespace).unwrap()[0].0, "foo");
    }

    #[test]
    fn test_local_only() {
        let data_model = test_model();
        let mut dispatcher = ControllerDispatcher::new(data_model);
        let fake = Arc::new(FakeController::new_local("fake_result"));
        let namespace = "/foo/bar".to_string();
        dispatcher.add(Uuid::new_v4(), namespace.clone(), fake).unwrap();
        assert_eq!(
            dispatcher.query(ConnectionMode::Remote, namespace.clone(), json!("")).is_ok(),
            false
        );
        assert_eq!(
            dispatcher.query(ConnectionMode::Local, namespace.clone(), json!("")).is_ok(),
            true
        );
    }
}
