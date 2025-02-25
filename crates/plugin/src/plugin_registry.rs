use crate::plugin::*;
use crate::plugin_error::PluginError;
use moon_env::MoonEnvironment;
use moon_pdk_api::MoonContext;
use proto_core::{is_offline, ProtoEnvironment};
use scc::hash_map::OccupiedEntry;
use scc::HashMap;
use starbase_utils::fs;
use std::fmt;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tracing::{debug, instrument};
use warpgate::{
    host::*, inject_default_manifest_config, to_virtual_path, Id, PluginContainer, PluginLoader,
    PluginLocator, PluginManifest, Wasm,
};

#[allow(dead_code)]
pub struct PluginRegistry<T: Plugin> {
    pub moon_env: Arc<MoonEnvironment>,
    pub proto_env: Arc<ProtoEnvironment>,

    loader: PluginLoader,
    plugins: Arc<HashMap<Id, T>>,
    type_of: PluginType,
    virtual_paths: BTreeMap<PathBuf, PathBuf>,
}

impl<T: Plugin> PluginRegistry<T> {
    pub fn new(
        type_of: PluginType,
        moon_env: Arc<MoonEnvironment>,
        proto_env: Arc<ProtoEnvironment>,
    ) -> Self {
        debug!(plugin = type_of.get_label(), "Creating plugin registry");

        // Create the loader
        let mut loader = PluginLoader::new(
            moon_env.plugins_dir.join(type_of.get_dir_name()),
            &moon_env.temp_dir,
        );

        loader.set_offline_checker(is_offline);

        // Merge proto and moon virtual paths
        let mut paths = BTreeMap::new();
        paths.extend(proto_env.get_virtual_paths());
        paths.extend(moon_env.get_virtual_paths());

        Self {
            loader,
            plugins: Arc::new(HashMap::default()),
            moon_env,
            proto_env,
            type_of,
            virtual_paths: paths,
        }
    }

    pub fn create_context(&self) -> MoonContext {
        MoonContext {
            working_dir: to_virtual_path(self.get_virtual_paths(), &self.moon_env.working_dir),
            workspace_root: to_virtual_path(
                self.get_virtual_paths(),
                &self.moon_env.workspace_root,
            ),
        }
    }

    pub fn create_manifest(&self, id: &Id, wasm_file: PathBuf) -> miette::Result<PluginManifest> {
        debug!(
            plugin = self.type_of.get_label(),
            id = id.as_str(),
            path = ?wasm_file,
            "Creating plugin manifest from WASM file",
        );

        let mut manifest = PluginManifest::new([Wasm::file(wasm_file)]);

        // Allow all hosts because we don't know what endpoints plugins
        // will communicate with. Far too many to account for.
        manifest = manifest.with_allowed_host("*");

        // Inherit moon and proto virtual paths.
        manifest = manifest.with_allowed_paths(self.virtual_paths.clone().into_iter());

        // Disable timeouts as some functions, like dependency installs,
        // may take multiple minutes to complete. We also can't account
        // for network connectivity.
        manifest.timeout_ms = None;

        // Inherit default configs, like host environment and ID.
        inject_default_manifest_config(id, &self.moon_env.home, &mut manifest)?;

        // Ensure virtual host paths exist, otherwise WASI (via extism)
        // will throw a cryptic file/directory not found error.
        for host_path in self.virtual_paths.keys() {
            fs::create_dir_all(host_path)?;
        }

        Ok(manifest)
    }

    pub fn get_cache(&self) -> Arc<HashMap<Id, T>> {
        Arc::clone(&self.plugins)
    }

    pub fn get_virtual_paths(&self) -> &BTreeMap<PathBuf, PathBuf> {
        &self.virtual_paths
    }

    pub async fn get_instance(&self, id: &Id) -> miette::Result<PluginInstance<T>> {
        Ok(self
            .plugins
            .get_async(id)
            .await
            .map(|entry| PluginInstance { entry })
            .ok_or_else(|| PluginError::UnknownId {
                id: id.to_string(),
                ty: self.type_of,
            })?)
    }

    pub fn is_registered(&self, id: &Id) -> bool {
        self.plugins.contains(id)
    }

    #[instrument(skip(self, op))]
    pub async fn load_with_config<I, L, F>(
        &self,
        id: I,
        locator: L,
        mut op: F,
    ) -> miette::Result<()>
    where
        I: AsRef<Id> + Debug,
        L: AsRef<PluginLocator> + Debug,
        F: FnMut(&mut PluginManifest) -> miette::Result<()>,
    {
        let id = id.as_ref();

        if self.plugins.contains(id) {
            return Err(PluginError::ExistingId {
                id: id.to_string(),
                ty: self.type_of,
            }
            .into());
        }

        debug!(
            plugin = self.type_of.get_label(),
            id = id.as_str(),
            "Attempting to load and register plugin",
        );

        // Load the WASM file (this must happen first because of async)
        let plugin_file = self.loader.load_plugin(id, locator).await?;

        // Create host functions (provided by warpgate)
        let functions = create_host_functions(HostData {
            http_client: self.loader.get_client()?.clone(),
            virtual_paths: self.virtual_paths.clone(),
            working_dir: self.moon_env.working_dir.clone(),
        });

        // Create the manifest and let the consumer configure it
        let mut manifest = self.create_manifest(id, plugin_file)?;

        op(&mut manifest)?;

        debug!(
            plugin = self.type_of.get_label(),
            id = id.as_str(),
            "Updated plugin manifest, attempting to register plugin",
        );

        // Combine everything into the container and register
        self.register(
            id.to_owned(),
            T::new(PluginRegistration {
                container: PluginContainer::new(id.to_owned(), manifest, functions)?,
                id: id.to_owned(),
                moon_env: Arc::clone(&self.moon_env),
                proto_env: Arc::clone(&self.proto_env),
            })
            .await?,
        );

        Ok(())
    }

    pub async fn load_without_config<I, L>(&self, id: I, locator: L) -> miette::Result<()>
    where
        I: AsRef<Id> + Debug,
        L: AsRef<PluginLocator> + Debug,
    {
        self.load_with_config(id, locator, |_| Ok(())).await
    }

    pub fn register(&self, id: Id, plugin: T) {
        debug!(
            plugin = self.type_of.get_label(),
            id = id.as_str(),
            "Registered plugin",
        );

        let _ = self.plugins.insert(id, plugin);
    }
}

impl<T: Plugin> fmt::Debug for PluginRegistry<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("moon_env", &self.moon_env)
            .field("proto_env", &self.proto_env)
            .field("plugins", &self.plugins)
            .field("type_of", &self.type_of)
            .field("virtual_paths", &self.virtual_paths)
            .finish()
    }
}
pub struct PluginInstance<'l, T: Plugin> {
    entry: OccupiedEntry<'l, Id, T>,
}

impl<'l, T: Plugin> Deref for PluginInstance<'l, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.entry.get()
    }
}

impl<'l, T: Plugin> DerefMut for PluginInstance<'l, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.entry.get_mut()
    }
}
