use crate::lan_api::Client as LanClient;
use crate::platform_api::GoveeApiClient;
use crate::service::device::Device;
use crate::service::hass::spawn_hass_integration;
use crate::service::http::run_http_server;
use crate::service::iot::start_iot_client;
use crate::service::state::{SavedRoom, StateHandle};
use crate::undoc_api::GoveeUndocumentedApi;
use crate::version_info::govee_version;
use crate::UndocApiArguments;
use anyhow::Context;
use chrono::Utc;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{sleep, timeout, Duration};

pub static POLL_INTERVAL: Lazy<chrono::Duration> = Lazy::new(|| chrono::Duration::seconds(900));

#[derive(clap::Parser, Debug)]
pub struct ServeCommand {
    /// The port on which the HTTP API will listen
    #[arg(long, default_value_t = 8056)]
    http_port: u16,
}

async fn poll_single_device(state: &StateHandle, device: &Device) -> anyhow::Result<()> {
    let now = Utc::now();

    if device.is_ble_only_device() == Some(true) {
        // We can't poll this device, we have no ble support
        return Ok(());
    }

    // Collect the device status via the LAN API, if possible.
    // This is partially redundant with the LAN discovery task,
    // but the timing of that is not as regular and predictable
    // because it employs exponential backoff.
    // Some Govee devices have bad firmware that will cause the
    // lights to flicker about a minute after polling, so it
    // is desirable to keep polling on a regular basis.
    // <https://github.com/wez/govee2mqtt/issues/250>
    if let Some(lan_device) = &device.lan_device {
        if let Some(client) = state.get_lan_client().await {
            if let Ok(status) = client.query_status(lan_device).await {
                state
                    .device_mut(&lan_device.sku, &lan_device.device)
                    .await
                    .set_lan_device_status(status);
                state.notify_of_state_change(&lan_device.device).await.ok();
            }
        }
    }

    let poll_interval = device.preferred_poll_interval();

    let can_update = match &device.last_polled {
        None => true,
        Some(last) => now - last > poll_interval,
    };

    if !can_update {
        return Ok(());
    }

    let device_state = device.device_state();
    let needs_update = match &device_state {
        None => true,
        Some(state) => now - state.updated > poll_interval,
    };

    if !needs_update {
        return Ok(());
    }

    let needs_platform = device.needs_platform_poll();

    // Don't interrogate via HTTP if we can use the LAN.
    // If we have LAN and the device is stale, it is likely
    // offline and there is little sense in burning up request
    // quota to the platform API for it
    if device.lan_device.is_some() && !needs_platform {
        log::trace!("LAN-available device {device} needs a status update; it's likely offline.");
        return Ok(());
    }

    if !needs_platform && state.poll_iot_api(device).await? {
        return Ok(());
    }

    state.poll_platform_api(device).await?;

    Ok(())
}

async fn periodic_state_poll(state: StateHandle) -> anyhow::Result<()> {
    sleep(Duration::from_secs(20)).await;
    loop {
        for d in state.devices().await {
            if let Err(err) = poll_single_device(&state, &d).await {
                log::error!("while polling {d}: {err:#}");
            }
        }

        sleep(Duration::from_secs(30)).await;
    }
}

async fn enumerate_devices_via_platform_api(
    state: StateHandle,
    client: Option<GoveeApiClient>,
) -> anyhow::Result<()> {
    let client = match client {
        Some(client) => client,
        None => match state.get_platform_client().await {
            Some(client) => client,
            None => return Ok(()),
        },
    };

    log::info!("Querying platform API for device list");
    let devices = client.get_devices().await?;
    state.save_offline("platform-devices-v1", &devices).await?;
    for info in devices {
        let mut device = state.device_mut(&info.sku, &info.device).await;
        device.set_http_device_info(info);
    }
    Ok(())
}

async fn enumerate_devices_via_undo_api(
    state: StateHandle,
    client: Option<GoveeUndocumentedApi>,
    args: &UndocApiArguments,
) -> anyhow::Result<()> {
    let client = match client {
        Some(client) => client,
        None => match state.get_undoc_client().await {
            Some(client) => client,
            None => return Ok(()),
        },
    };

    log::info!("Querying undocumented API for device + room list");
    let acct = client.login_account_cached().await?;
    let info = client.get_device_list(&acct.token).await?;
    let mut group_by_id = HashMap::new();
    for group in info.groups {
        group_by_id.insert(group.group_id, group.group_name);
    }
    let rooms: Vec<SavedRoom> = info
        .devices
        .iter()
        .map(|entry| SavedRoom {
            sku: entry.sku.clone(),
            device: entry.device.clone(),
            room_name: group_by_id.get(&entry.group_id).cloned(),
        })
        .collect();
    state.save_offline("rooms-v1", &rooms).await?;
    for entry in info.devices {
        let mut device = state.device_mut(&entry.sku, &entry.device).await;
        let room_name = group_by_id.get(&entry.group_id).map(|name| name.as_str());
        device.set_undoc_device_info(entry, room_name);
    }

    if state.get_iot_client().await.is_none() {
        start_iot_client(args, state.clone(), Some(acct)).await?;
    }
    Ok(())
}

const CLOUD_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

async fn refresh_saved_scenes(state: &StateHandle) {
    match timeout(CLOUD_DISCOVERY_TIMEOUT, state.refresh_one_clicks()).await {
        Ok(Ok(true)) => {
            if let Some(hass) = state.get_hass_client().await {
                if let Err(err) = hass.register_with_hass(state).await {
                    log::warn!("Registering refreshed One-Click scenes: {err:#}");
                }
            }
        }
        Ok(Ok(false)) => {}
        Ok(Err(err)) => log::warn!("One-Click refresh failed; keeping saved scenes: {err:#}"),
        Err(err) => log::warn!("One-Click refresh timed out; keeping saved scenes: {err}"),
    }
}

impl ServeCommand {
    pub async fn run(&self, args: &crate::Args) -> anyhow::Result<()> {
        log::info!("Starting service. version {}", govee_version());
        let state = Arc::new(crate::service::state::State::new());
        let cache_dir = crate::cache::cache_dir();
        std::fs::create_dir_all(&cache_dir)?;
        state
            .restore_offline(&cache_dir.join("govee2mqtt-offline.sqlite"))
            .await?;

        // First, use the HTTP APIs to determine the list of devices and
        // their names.

        if let Ok(client) = args.api_args.api_client() {
            match timeout(
                CLOUD_DISCOVERY_TIMEOUT,
                enumerate_devices_via_platform_api(state.clone(), Some(client.clone())),
            )
            .await
            {
                Ok(Ok(())) => state.set_platform_client(client.clone()).await,
                result => log::warn!(
                    "Initial platform discovery failed; using saved metadata and LAN: {result:?}"
                ),
            }

            // spawn periodic discovery task
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(600)).await;
                    match timeout(
                        CLOUD_DISCOVERY_TIMEOUT,
                        enumerate_devices_via_platform_api(state.clone(), Some(client.clone())),
                    )
                    .await
                    {
                        Ok(Ok(())) => state.set_platform_client(client.clone()).await,
                        result => log::warn!(
                            "Periodic platform discovery failed; retaining metadata: {result:?}"
                        ),
                    }
                }
            });
        }
        if let Ok(client) = args.undoc_args.api_client() {
            state.set_undoc_client(client.clone()).await;
            if let result @ (Ok(Err(_)) | Err(_)) = timeout(
                CLOUD_DISCOVERY_TIMEOUT,
                enumerate_devices_via_undo_api(
                    state.clone(),
                    Some(client.clone()),
                    &args.undoc_args,
                ),
            )
            .await
            {
                log::warn!(
                    "Initial account discovery failed; using saved metadata and LAN: {result:?}"
                );
            }
            refresh_saved_scenes(&state).await;

            // spawn periodic discovery task
            let state = state.clone();
            let args = args.undoc_args.clone();
            tokio::spawn(async move {
                loop {
                    sleep(Duration::from_secs(600)).await;
                    if let result @ (Ok(Err(_)) | Err(_)) = timeout(
                        CLOUD_DISCOVERY_TIMEOUT,
                        enumerate_devices_via_undo_api(state.clone(), None, &args),
                    )
                    .await
                    {
                        log::warn!(
                            "Periodic account discovery failed; retaining metadata: {result:?}"
                        );
                    }
                    refresh_saved_scenes(&state).await;
                }
            });
        }

        // Now start LAN discovery

        let options = args.lan_disco_args.to_disco_options()?;
        if !options.is_empty() {
            log::info!("Starting LAN discovery");
            let state = state.clone();
            let (client, mut scan) = LanClient::new(options).await?;

            state.set_lan_client(client.clone()).await;

            tokio::spawn(async move {
                while let Some(lan_device) = scan.recv().await {
                    log::trace!("LAN disco: {lan_device:?}");
                    state
                        .device_mut(&lan_device.sku, &lan_device.device)
                        .await
                        .set_lan_device(lan_device.clone());

                    let state = state.clone();
                    let client = client.clone();
                    tokio::spawn(async move {
                        if let Ok(status) = client.query_status(&lan_device).await {
                            state
                                .device_mut(&lan_device.sku, &lan_device.device)
                                .await
                                .set_lan_device_status(status);

                            log::trace!("LAN disco: update and notify {}", lan_device.device);
                            state.notify_of_state_change(&lan_device.device).await.ok();
                        }
                    });
                }
            });

            // I don't love that this is 10 seconds but since our timeout
            // for query_status is 10 seconds, and we show a warning for
            // devices that didn't respond in the section below, in the
            // interest of reducing false positives we need to wait long
            // enough to provide high-signal warnings.
            log::info!("Waiting 10 seconds for LAN API discovery");
            sleep(Duration::from_secs(10)).await;
        }

        log::info!("Devices returned from Govee's APIs");
        for device in state.devices().await {
            log::info!("{device}");
            if let Some(lan) = &device.lan_device {
                log::info!("  LAN API: ip={:?}", lan.ip);
            }
            if let Some(http_info) = &device.http_device_info {
                let kind = &http_info.device_type;
                let rgb = http_info.supports_rgb();
                let bright = http_info.supports_brightness();
                let color_temp = http_info.get_color_temperature_range();
                let segment_rgb = http_info.supports_segmented_rgb();
                log::info!(
                    "  Platform API: {kind}. supports_rgb={rgb} supports_brightness={bright}"
                );
                log::info!("                color_temp={color_temp:?} segment_rgb={segment_rgb:?}");
                log::trace!("{http_info:#?}");
            }
            if let Some(undoc) = &device.undoc_device_info {
                let room = &undoc.room_name;
                let supports_iot = undoc.entry.device_ext.device_settings.topic.is_some();
                let ble_only = undoc.entry.device_ext.device_settings.wifi_name.is_none();
                log::info!(
                    "  Undoc: room={room:?} supports_iot={supports_iot} ble_only={ble_only}"
                );
                log::trace!("{undoc:#?}");
            }
            if let Some(quirk) = device.resolve_quirk() {
                log::info!("  {quirk:?}");

                // Sanity check for LAN devices: if we don't see an API for it,
                // it may indicate a networking issue
                if quirk.lan_api_capable && device.lan_device.is_none() {
                    log::warn!(
                        "  This device should be available via the LAN API, \
                        but didn't respond to probing yet. Possible causes:"
                    );
                    log::warn!("  1) LAN API needs to be enabled in the Govee Home App.");
                    log::warn!("  2) The device is offline.");
                    log::warn!("  3) A network configuration issue is preventing communication.");
                    log::warn!(
                        "  4) The device needs a firmware update before it can enable LAN API."
                    );
                    log::warn!(
                        "  5) The hardware version of the device is too old to enable the LAN API."
                    );
                }
            } else if device.http_device_info.is_none() {
                log::warn!("  Unknown device type. Cannot map to Home Assistant.");
                if state.get_platform_client().await.is_none() {
                    log::warn!(
                        "  Recommendation: configure your Govee API Key so that \
                                  metadata can be fetched from Govee"
                    );
                }
            }

            log::info!("");
        }

        // Start periodic status polling
        {
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = periodic_state_poll(state).await {
                    log::error!("periodic_state_poll: {err:#}");
                }
            });
        }

        // start advertising on local mqtt
        spawn_hass_integration(state.clone(), &args.hass_args).await?;

        run_http_server(state.clone(), self.http_port)
            .await
            .with_context(|| format!("Starting HTTP service on port {}", self.http_port))
    }
}
