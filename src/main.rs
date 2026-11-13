//! CyclAI plugin for Garmin Connect. A small HTTP service: CyclAI asks it for
//! the activity list and for each activity's original `.fit`; it needs no
//! access to CyclAI, and CyclAI needs no Garmin credentials.
//!
//! Environment:
//! - `CYCLAI_PLUGIN_SECRET`: secret shared with CyclAI (same value as its
//!   `CYCLAI_PLUGIN_GARMIN_SECRET`), at least 16 characters.
//! - `GARMIN_API_KEY`: personal API key (Garmin Connect > Paramètres > API personnelle).
//! - `BIND_ADDR`: listen address, default `0.0.0.0:8080`.

mod garmin;

use async_trait::async_trait;
use cyclai_plugin_sdk::{Fetched, ListedActivity, Manifest, Plugin, PluginError, PROTOCOL_VERSION};
use std::sync::Arc;

struct GarminPlugin {
    garmin: garmin::Garmin,
}

#[async_trait]
impl Plugin for GarminPlugin {
    async fn manifest(&self) -> Manifest {
        let configured = self.garmin.is_configured();
        Manifest {
            protocol: PROTOCOL_VERSION,
            id: "garmin".into(),
            label: "Garmin Connect".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            sports: ["Ride", "GravelRide", "VirtualRide", "Run", "TrailRun"].map(String::from).to_vec(),
            configured,
            message: (!configured).then(|| "définis GARMIN_API_KEY dans le conteneur du plugin".to_string()),
            activity_url: Some("https://connect.garmin.com/modern/activity/{id}".into()),
            activity_url_label: Some("voir sur Garmin Connect".into()),
        }
    }

    async fn list(&self, after_epoch: i64) -> Result<Vec<ListedActivity>, PluginError> {
        if !self.garmin.is_configured() {
            return Err(PluginError::not_configured("GARMIN_API_KEY n'est pas défini"));
        }
        self.garmin.list(after_epoch).await
    }

    async fn fetch(&self, external_id: &str) -> Result<Fetched, PluginError> {
        if !self.garmin.is_configured() {
            return Err(PluginError::not_configured("GARMIN_API_KEY n'est pas défini"));
        }
        Ok(match self.garmin.download_fit(external_id).await? {
            Some(fit) => Fetched::Fit(fit),
            None => Fetched::NotAvailable,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let secret = std::env::var("CYCLAI_PLUGIN_SECRET").unwrap_or_default();
    let addr: std::net::SocketAddr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()).parse()?;
    let garmin = garmin::Garmin::new(std::env::var("GARMIN_API_KEY").unwrap_or_default())?;
    if !garmin.is_configured() {
        tracing::warn!("GARMIN_API_KEY is not set: the plugin answers but reports itself as not configured");
    }
    tracing::info!(%addr, "Garmin plugin listening");
    cyclai_plugin_sdk::serve(Arc::new(GarminPlugin { garmin }), addr, secret).await
}
