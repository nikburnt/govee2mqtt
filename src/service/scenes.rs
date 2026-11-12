use crate::lan_api::{DeviceColor, LanDevice, Request};
use crate::service::state::State;
use crate::undoc_api::{OneClick, OneClickComponent};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalScene {
    pub name: String,
    actions: Vec<LocalAction>,
    pub unsupported: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LocalAction {
    device: String,
    sku: String,
    request: Request,
}

fn local_request(value: &Value) -> anyhow::Result<Request> {
    let msg = value.get("msg").context("missing command envelope")?;
    let cmd = msg["cmd"].as_str().context("missing command name")?;
    let data = msg.get("data").context("missing command data")?;
    match cmd {
        "ptReal" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Data {
                command: Vec<String>,
            }
            let data: Data = serde_json::from_value(data.clone())?;
            ensure!(!data.command.is_empty(), "empty ptReal command");
            for frame in &data.command {
                ensure!(
                    !data_encoding::BASE64.decode(frame.as_bytes())?.is_empty(),
                    "empty ptReal frame"
                );
            }
            Ok(Request::PtReal {
                command: data.command,
            })
        }
        "turn" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Data {
                val: u8,
            }
            let data: Data = serde_json::from_value(data.clone())?;
            ensure!(data.val <= 1, "unsupported power value {}", data.val);
            Ok(Request::Turn { value: data.val })
        }
        "brightness" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Data {
                val: u8,
            }
            let data: Data = serde_json::from_value(data.clone())?;
            ensure!(data.val <= 100, "brightness is not a percentage");
            Ok(Request::Brightness { value: data.val })
        }
        "colorwc" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Color {
                r: u8,
                g: u8,
                b: u8,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Data {
                color: Color,
                color_tem_in_kelvin: u32,
            }
            let data: Data = serde_json::from_value(data.clone())?;
            ensure!(
                data.color_tem_in_kelvin == 0 || (1000..=10000).contains(&data.color_tem_in_kelvin),
                "unsupported color temperature"
            );
            Ok(Request::Color {
                color: DeviceColor {
                    r: data.color.r,
                    g: data.color.g,
                    b: data.color.b,
                },
                color_temperature_kelvin: data.color_tem_in_kelvin,
            })
        }
        _ => anyhow::bail!("unsupported LAN scene command {cmd}"),
    }
}

impl LocalScene {
    fn from_one_click(group: &str, scene: OneClick) -> Self {
        let name = format!("One-Click: {group}: {}", scene.name);
        let parse = || -> anyhow::Result<Vec<LocalAction>> {
            ensure!(scene.exec_rules.is_empty(), "unsupported execution rules");
            let mut actions = vec![];
            for rule in scene.iot_rules {
                let device = rule.device_obj.device.context("missing device ID")?;
                let sku = rule.device_obj.sku.context("missing device model")?;
                ensure!(
                    !device.is_empty() && !sku.is_empty(),
                    "empty device identity"
                );
                ensure!(!rule.rule.is_empty(), "device has no commands");
                for command in rule.rule {
                    actions.push(LocalAction {
                        device: device.clone(),
                        sku: sku.clone(),
                        request: local_request(&command.iot_msg)
                            .with_context(|| format!("device {sku} {device}"))?,
                    });
                }
            }
            ensure!(!actions.is_empty(), "scene has no local device commands");
            Ok(actions)
        };
        match parse() {
            Ok(actions) => Self {
                name,
                actions,
                unsupported: None,
            },
            Err(err) => Self {
                name,
                actions: vec![],
                unsupported: Some(format!("{err:#}")),
            },
        }
    }
}

fn merge_scenes(
    saved: &BTreeMap<String, LocalScene>,
    groups: Vec<OneClickComponent>,
) -> anyhow::Result<BTreeMap<String, LocalScene>> {
    let mut received = BTreeMap::new();
    for group in groups {
        for scene in group.one_clicks {
            if scene.iot_rules.is_empty()
                && scene.exec_rules.is_empty()
                && !saved.contains_key(&format!("One-Click: {}: {}", group.name, scene.name))
            {
                continue;
            }
            let scene = LocalScene::from_one_click(&group.name, scene);
            ensure!(
                !received.contains_key(&scene.name),
                "ambiguous duplicate scene name {}",
                scene.name
            );
            received.insert(scene.name.clone(), scene);
        }
    }
    let mut merged = saved.clone();
    merged.extend(received);
    Ok(merged)
}

impl State {
    pub async fn saved_one_clicks(&self) -> Vec<LocalScene> {
        self.one_clicks.lock().await.values().cloned().collect()
    }

    pub async fn refresh_one_clicks(&self) -> anyhow::Result<bool> {
        let Some(client) = self.get_undoc_client().await else {
            return Ok(false);
        };
        let groups = client.fetch_one_click_shortcuts().await?;
        self.store_one_clicks(groups).await
    }

    async fn store_one_clicks(&self, groups: Vec<OneClickComponent>) -> anyhow::Result<bool> {
        let mut saved = self.one_clicks.lock().await;
        let merged = merge_scenes(&saved, groups)?;
        let changed = serde_json::to_value(&*saved)? != serde_json::to_value(&merged)?;
        self.save_offline("one-clicks-v1", &merged).await?;
        if changed {
            for scene in merged.values() {
                if let Some(reason) = &scene.unsupported {
                    log::warn!("{} is not locally executable: {reason}", scene.name);
                }
            }
            log::info!(
                "Saved {} One-Click scenes for local execution",
                merged.len()
            );
        }
        *saved = merged;
        Ok(changed)
    }

    async fn prepare_one_click(&self, name: &str) -> anyhow::Result<Vec<(LanDevice, Request)>> {
        let scene = self
            .one_clicks
            .lock()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("no saved One-Click scene {name}"))?;
        if let Some(reason) = &scene.unsupported {
            anyhow::bail!("{name} cannot run locally: {reason}");
        }
        let mut commands = vec![];
        for action in scene.actions {
            let device = self
                .device_by_id(&action.device)
                .await
                .with_context(|| format!("scene device {} is not discovered", action.device))?;
            ensure!(device.sku == action.sku, "scene device model changed");
            let lan = device.lan_device.with_context(|| {
                format!("scene device {} is not available via LAN", action.device)
            })?;
            commands.push((lan, action.request));
        }
        ensure!(!commands.is_empty(), "scene has no commands");
        Ok(commands)
    }

    pub async fn activate_one_click(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        // Validate every action and target before sending anything, then take
        // device locks in a stable order to avoid interleaving concurrent scenes.
        let commands = self.prepare_one_click(name).await?;
        let targets: std::collections::BTreeSet<_> = commands
            .iter()
            .map(|(device, _)| device.device.clone())
            .collect();
        let mut guards = vec![];
        for device in &targets {
            guards.push(self.resolve_device_for_control(device).await?);
        }
        log::info!("Executing {name} locally ({} commands)", commands.len());
        for (device, request) in commands {
            device.send_request(request).await?;
        }
        // UDP send success is not an acknowledgement of the rendered effect.
        // Coordinator drop schedules normal state reconciliation.
        drop(guards);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undoc_api::OneClickResponse;
    use serde_json::json;

    fn fixture() -> Vec<OneClickComponent> {
        serde_json::from_str::<OneClickResponse>(include_str!(
            "../../test-data/undoc-one-click.json"
        ))
        .unwrap()
        .data
        .components
    }

    fn fixture_scene(groups: &mut [OneClickComponent]) -> &mut OneClick {
        groups
            .iter_mut()
            .flat_map(|g| &mut g.one_clicks)
            .find(|s| s.name == "woot")
            .unwrap()
    }

    #[test]
    fn imports_commands_without_executing_them() -> anyhow::Result<()> {
        let scenes = merge_scenes(&BTreeMap::new(), fixture())?;
        let scene = scenes.values().find(|s| s.name.ends_with("woot")).unwrap();
        assert!(scene.unsupported.is_none());
        assert!(matches!(
            scene.actions[0].request,
            Request::Turn { value: 1 }
        ));
        assert!(matches!(scene.actions[1].request, Request::PtReal { .. }));
        let serialized = serde_json::to_string(&scenes)?;
        assert!(!serialized.contains("accountTopic"));
        assert!(!serialized.contains("transaction"));
        Ok(())
    }

    #[test]
    fn refresh_replaces_existing_commands_and_preserves_absent_scenes() -> anyhow::Result<()> {
        let first = merge_scenes(&BTreeMap::new(), fixture())?;
        let mut groups = fixture();
        fixture_scene(&mut groups).iot_rules[0].rule[0].iot_msg["msg"]["data"]["val"] = json!(0);
        let updated = merge_scenes(&first, groups)?;
        let scene = updated.values().find(|s| s.name.ends_with("woot")).unwrap();
        assert!(matches!(
            scene.actions[0].request,
            Request::Turn { value: 0 }
        ));
        assert_eq!(merge_scenes(&updated, vec![])?.len(), updated.len());
        Ok(())
    }

    #[test]
    fn imports_h61f5_brightness_and_color_temperature() -> anyhow::Result<()> {
        for (brightness, kelvin, color) in [
            (10, 2700, json!({"r":255,"g":174,"b":84})),
            (100, 6100, json!({"r":255,"g":245,"b":240})),
        ] {
            let mut groups = fixture();
            let rule = &mut fixture_scene(&mut groups).iot_rules[0];
            rule.device_obj.sku = Some("H61F5".to_string());
            rule.rule.truncate(1);
            let mut entry = rule.rule[0].clone();
            entry.iot_msg = json!({"msg":{"cmd":"brightness","data":{"val":brightness}}});
            rule.rule.push(entry.clone());
            entry.iot_msg = json!({"msg":{"cmd":"colorwc","data":{
                "color":color,"colorTemInKelvin":kelvin
            }}});
            rule.rule.push(entry);
            let scenes = merge_scenes(&BTreeMap::new(), groups)?;
            let scene = scenes.values().find(|s| s.name.ends_with("woot")).unwrap();
            assert!(scene.unsupported.is_none(), "{:?}", scene.unsupported);
            assert_eq!(scene.actions.len(), 3);
            assert_eq!(
                serde_json::to_value(&scene.actions[1].request)?,
                json!({"cmd":"brightness","data":{"value":brightness}})
            );
            assert_eq!(
                serde_json::to_value(&scene.actions[2].request)?,
                json!({"cmd":"colorwc","data":{"color":color,"colorTemInKelvin":kelvin}})
            );
        }
        Ok(())
    }

    #[test]
    fn imports_percentage_brightness_without_a_model_allowlist() -> anyhow::Result<()> {
        for sku in ["H6076", "H61F5", "unlisted-model"] {
            for brightness in [0, 10, 100] {
                let mut groups = fixture();
                let rule = &mut fixture_scene(&mut groups).iot_rules[0];
                rule.device_obj.sku = Some(sku.to_string());
                let mut entry = rule.rule[0].clone();
                entry.iot_msg = json!({"msg":{"cmd":"brightness","data":{"val":brightness}}});
                rule.rule.push(entry);
                let scenes = merge_scenes(&BTreeMap::new(), groups)?;
                let scene = scenes.values().find(|s| s.name.ends_with("woot")).unwrap();
                assert!(
                    scene.unsupported.is_none(),
                    "{sku}: {:?}",
                    scene.unsupported
                );
                assert_eq!(scene.actions.len(), 3);
                assert_eq!(
                    serde_json::to_value(&scene.actions[2].request)?,
                    json!({"cmd":"brightness","data":{"value":brightness}})
                );
            }
        }
        Ok(())
    }

    #[test]
    fn unknown_action_invalidates_the_entire_updated_scene() -> anyhow::Result<()> {
        let first = merge_scenes(&BTreeMap::new(), fixture())?;
        let mut groups = fixture();
        fixture_scene(&mut groups).iot_rules[0].rule[1].iot_msg["msg"]["cmd"] = json!("unknown");
        let updated = merge_scenes(&first, groups)?;
        let scene = updated.values().find(|s| s.name.ends_with("woot")).unwrap();
        assert!(scene.unsupported.is_some());
        assert!(scene.actions.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_invalid_frames_power_values_and_unsupported_command_shapes() {
        for msg in [
            json!({"cmd":"ptReal","data":{"command":["invalid"]}}),
            json!({"cmd":"ptReal","data":{"command":[]}}),
            json!({"cmd":"turn","data":{"val":17}}),
            json!({"cmd":"turn","data":{"val":1,"channel":2}}),
            json!({"cmd":"brightness","data":{"val":254}}),
            json!({"cmd":"brightness","data":{"val":10,"channel":2}}),
            json!({"cmd":"colorwc","data":{"color":{"r":256,"g":0,"b":0},"colorTemInKelvin":0}}),
            json!({"cmd":"colorwc","data":{"color":{"r":1,"g":0,"b":0,"channel":2},"colorTemInKelvin":0}}),
            json!({"cmd":"colorwc","data":{"color":{"r":1,"g":0,"b":0},"colorTemInKelvin":50000}}),
        ] {
            assert!(local_request(&json!({"msg":msg})).is_err());
        }
    }

    #[tokio::test]
    async fn offline_scene_requires_every_target_to_be_discovered() -> anyhow::Result<()> {
        let state = Arc::new(State::new());
        let scenes = merge_scenes(&BTreeMap::new(), fixture())?;
        let name = scenes.keys().find(|n| n.ends_with("woot")).unwrap().clone();
        *state.one_clicks.lock().await = scenes;
        assert!(state.prepare_one_click(&name).await.is_err());
        assert!(state.get_undoc_client().await.is_none());
        assert!(state.get_iot_client().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn scene_refresh_is_durable_and_invalid_refresh_keeps_last_good_copy(
    ) -> anyhow::Result<()> {
        let path =
            std::env::temp_dir().join(format!("govee-scenes-{}.sqlite", uuid::Uuid::new_v4()));
        {
            let state = State::new();
            state.restore_offline(&path).await?;
            assert!(state.store_one_clicks(fixture()).await?);
            assert!(!state.store_one_clicks(fixture()).await?);
            let mut updated = fixture();
            fixture_scene(&mut updated).iot_rules[0].rule[0].iot_msg["msg"]["data"]["val"] =
                json!(0);
            assert!(state.store_one_clicks(updated.clone()).await?);
            let prior = serde_json::to_value(state.saved_one_clicks().await)?;
            updated.extend(updated.clone());
            assert!(state.store_one_clicks(updated).await.is_err());
            assert_eq!(serde_json::to_value(state.saved_one_clicks().await)?, prior);
            assert!(!state.refresh_one_clicks().await?); // No cloud credentials.
        }
        {
            let state = State::new();
            state.restore_offline(&path).await?;
            let scenes = state.saved_one_clicks().await;
            let scene = scenes.iter().find(|s| s.name.ends_with("woot")).unwrap();
            assert!(matches!(
                scene.actions[0].request,
                Request::Turn { value: 0 }
            ));
        }
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn offline_execution_sends_only_saved_lan_commands_in_order() -> anyhow::Result<()> {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:4003").await?;
        let state = Arc::new(State::new());
        let scenes = merge_scenes(&BTreeMap::new(), fixture())?;
        let scene = scenes
            .values()
            .find(|s| s.name.ends_with("woot"))
            .unwrap()
            .clone();
        let action = &scene.actions[0];
        state
            .device_mut(&action.sku, &action.device)
            .await
            .set_lan_device(LanDevice {
                ip: "127.0.0.1".parse()?,
                sku: action.sku.clone(),
                device: action.device.clone(),
                ble_version_hard: String::new(),
                ble_version_soft: String::new(),
                wifi_version_hard: String::new(),
                wifi_version_soft: String::new(),
            });
        *state.one_clicks.lock().await = scenes;
        state.activate_one_click(&scene.name).await?;
        let mut buffer = [0u8; 8192];
        for action in &scene.actions {
            let (size, _) = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                receiver.recv_from(&mut buffer),
            )
            .await??;
            let packet: Value = serde_json::from_slice(&buffer[..size])?;
            assert_eq!(packet, json!({"msg": action.request}));
        }
        let mut incomplete = scene.clone();
        let mut missing = scene.actions[0].clone();
        missing.device = "missing".to_string();
        incomplete.actions.push(missing);
        state
            .one_clicks
            .lock()
            .await
            .insert(scene.name.clone(), incomplete);
        assert!(state.activate_one_click(&scene.name).await.is_err());
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(50),
            receiver.recv_from(&mut buffer)
        )
        .await
        .is_err());
        assert!(state.get_undoc_client().await.is_none());
        assert!(state.get_iot_client().await.is_none());
        Ok(())
    }
}
