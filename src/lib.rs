/*
 * This file is part of Edgehog.
 *
 * Copyright 2022 SECO Mind Srl
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use crate::data::astarte::Astarte;
use crate::data::Publisher;
use crate::ota::ota_handler::OTAHandler;
use crate::telemetry::TelemetryPayload;
use astarte_sdk::builder::AstarteOptions;
use astarte_sdk::types::AstarteType;
use astarte_sdk::{registration, Aggregation};
use device::DeviceProxy;
use error::DeviceManagerError;
use log::{debug, info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::RwLock;

mod commands;
mod data;
mod device;
pub mod error;
mod ota;
mod power_management;
mod repository;
mod telemetry;
pub mod wrapper;

#[derive(Debug, Deserialize)]
pub struct DeviceManagerOptions {
    pub realm: String,
    pub device_id: Option<String>,
    pub credentials_secret: Option<String>,
    pub pairing_url: String,
    pub pairing_token: Option<String>,
    pub interfaces_directory: String,
    pub state_file: String,
    pub download_directory: String,
    pub telemetry_config: Vec<telemetry::TelemetryInterfaceConfig>,
}

pub struct DeviceManager {
    astarte_publisher: Astarte,
    //we pass the ota event through a channel, to avoid blocking the main loop
    ota_event_channel: Sender<HashMap<String, AstarteType>>,
    telemetry: Arc<RwLock<telemetry::Telemetry>>,
}

impl DeviceManager {
    pub async fn new(opts: DeviceManagerOptions) -> Result<DeviceManager, DeviceManagerError> {
        let device_id: String = get_device_id(opts.device_id.clone()).await?;
        let credential_secret: String = get_credentials_secret(&device_id, &opts).await?;

        let sdk_options = AstarteOptions::new(
            &opts.realm,
            &device_id,
            &credential_secret,
            &opts.pairing_url,
        )
        .interface_directory(&opts.interfaces_directory)?
        .build();
        info!("Starting");

        wrapper::systemd::systemd_notify_status("Initializing");
        let astarte_client = Astarte::new(&sdk_options).await?;

        let mut ota_handler = OTAHandler::new(&opts).await?;

        ota_handler
            .ensure_pending_ota_response(&astarte_client)
            .await?;

        let (tx, mut rx) = channel(32);

        let astarte_client_clone = astarte_client.clone();
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                ota_handler
                    .ota_event(&astarte_client_clone, data)
                    .await
                    .ok();
            }
        });

        let (telemetry_tx, mut telemetry_rx) = channel(32);
        let astarte_client_telemetry_clone = astarte_client.clone();

        let tel =
            telemetry::Telemetry::from_default_config(opts.telemetry_config, telemetry_tx).await;
        tokio::spawn(async move {
            while let Some(msg) = telemetry_rx.recv().await {
                match msg {
                    TelemetryPayload::SystemStatus(data) => {
                        let _ = astarte_client_telemetry_clone
                            .send_object(
                                "io.edgehog.devicemanager.SystemStatus",
                                "/systemStatus",
                                data,
                            )
                            .await;
                    }
                };
            }
        });

        Ok(Self {
            astarte_publisher: astarte_client,
            telemetry: Arc::new(RwLock::new(tel)),
            ota_event_channel: tx,
        })
    }

    pub async fn run(&mut self) {
        wrapper::systemd::systemd_notify_status("Running");
        let tel_clone = self.telemetry.clone();
        tokio::task::spawn(async move {
            tel_clone.write().await.run_telemetry().await;
        });

        loop {
            match self.astarte_publisher.clone().device_sdk.poll().await {
                Ok(clientbound) => {
                    debug!("incoming: {:?}", clientbound);

                    match (
                        clientbound.interface.as_str(),
                        clientbound
                            .path
                            .trim_matches('/')
                            .split('/')
                            .collect::<Vec<&str>>()
                            .as_slice(),
                        &clientbound.data,
                    ) {
                        (
                            "io.edgehog.devicemanager.OTARequest",
                            ["request"],
                            Aggregation::Object(data),
                        ) => self.ota_event_channel.send(data.clone()).await.unwrap(),

                        (
                            "io.edgehog.devicemanager.Commands",
                            ["request"],
                            Aggregation::Individual(AstarteType::String(command)),
                        ) => commands::execute_command(command),

                        (
                            "io.edgehog.devicemanager.config.Telemetry",
                            ["request", interface_name, endpoint],
                            Aggregation::Individual(data),
                        ) => {
                            self.telemetry
                                .clone()
                                .write()
                                .await
                                .telemetry_config_event(interface_name, endpoint, data)
                                .await;
                        }

                        _ => {
                            warn!("Receiving data from an unknown path/interface: {clientbound:?}");
                        }
                    }
                }
                Err(err) => log::error!("{:?}", err),
            }
        }
    }

    pub async fn init(&self) -> Result<(), DeviceManagerError> {
        wrapper::systemd::systemd_notify_status("Sending initial telemetry");
        self.send_initial_telemetry().await?;

        Ok(())
    }

    pub async fn send_initial_telemetry(&self) -> Result<(), DeviceManagerError> {
        let device = &self.astarte_publisher;

        let data = [
            (
                "io.edgehog.devicemanager.OSInfo",
                telemetry::os_info::get_os_info()?,
            ),
            (
                "io.edgehog.devicemanager.HardwareInfo",
                telemetry::hardware_info::get_hardware_info()?,
            ),
            (
                "io.edgehog.devicemanager.RuntimeInfo",
                telemetry::runtime_info::get_runtime_info()?,
            ),
        ];

        for (ifc, fields) in data {
            for (path, data) in fields {
                device.device_sdk.send(ifc, &path, data).await?;
            }
        }

        Ok(())
    }
}

async fn get_device_id(opt_device_id: Option<String>) -> Result<String, DeviceManagerError> {
    if let Some(device_id) = opt_device_id {
        Ok(device_id)
    } else {
        get_hardware_id_from_dbus().await
    }
}

async fn get_hardware_id_from_dbus() -> Result<String, DeviceManagerError> {
    let connection = zbus::Connection::system().await?;
    let proxy = DeviceProxy::new(&connection).await?;
    let hardware_id: String = proxy.get_hardware_id("").await?;
    if hardware_id.is_empty() {
        return Err(DeviceManagerError::FatalError(
            "No hardware id provided".to_string(),
        ));
    }
    Ok(hardware_id)
}

async fn get_credentials_secret(
    device_id: &str,
    opts: &DeviceManagerOptions,
) -> Result<String, DeviceManagerError> {
    if let Some(secret) = opts.credentials_secret.clone() {
        Ok(secret)
    } else if Path::new(&format!("./{}.json", device_id)).exists() {
        get_credentials_secret_from_persistence(device_id)
    } else if let Some(token) = opts.pairing_token.clone() {
        get_credentials_secret_from_registration(device_id, &token, opts).await
    } else {
        Err(DeviceManagerError::FatalError(
            "Missing arguments".to_string(),
        ))
    }
}

fn get_credentials_secret_from_persistence(device_id: &str) -> Result<String, DeviceManagerError> {
    let reader = File::open(&format!("./{}.json", device_id)).unwrap();
    Ok(serde_json::from_reader(reader).expect("Unable to read secret"))
}

async fn get_credentials_secret_from_registration(
    device_id: &str,
    token: &str,
    opts: &DeviceManagerOptions,
) -> Result<String, DeviceManagerError> {
    let registration =
        registration::register_device(token, &opts.pairing_url, &opts.realm, &device_id).await;
    if let Ok(credential_secret) = registration {
        let writer = File::create(&format!("./{}.json", device_id)).unwrap();
        serde_json::to_writer(writer, &credential_secret).expect("Unable to write secret");
        Ok(credential_secret)
    } else {
        Err(DeviceManagerError::FatalError("Pairing error".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use crate::{get_credentials_secret, get_device_id, DeviceManagerOptions};

    #[tokio::test]
    async fn device_id_test() {
        assert_eq!(
            get_device_id(Some("target".to_string())).await.unwrap(),
            "target".to_string()
        );
    }

    #[tokio::test]
    async fn credentials_secret_test() {
        let options = DeviceManagerOptions {
            realm: "".to_string(),
            device_id: None,
            credentials_secret: Some("credentials_secret".to_string()),
            pairing_url: "".to_string(),
            pairing_token: None,
            interfaces_directory: "".to_string(),
            state_file: "".to_string(),
            download_directory: "".to_string(),
            telemetry_config: vec![],
        };
        assert_eq!(
            get_credentials_secret("device_id", &options).await.unwrap(),
            "credentials_secret".to_string()
        );
    }

    #[tokio::test]
    async fn not_enough_arguments_credentials_secret_test() {
        let options = DeviceManagerOptions {
            realm: "".to_string(),
            device_id: None,
            credentials_secret: None,
            pairing_url: "".to_string(),
            pairing_token: None,
            interfaces_directory: "".to_string(),
            state_file: "".to_string(),
            download_directory: "".to_string(),
            telemetry_config: vec![],
        };
        assert!(get_credentials_secret("device_id", &options).await.is_err());
    }
}
