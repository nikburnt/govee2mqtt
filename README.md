# Govee to MQTT bridge for Home Assistant

This repo provides a `govee` executable whose primary purpose is to act
as a bridge between [Govee](https://govee.com) devices and Home Assistant,
via the [Home Assistant MQTT Integration](https://www.home-assistant.io/integrations/mqtt/).

## Features

* Robust LAN-first design. Not all of Govee's devices support LAN control,
  but for those that do, you'll have the lowest latency and ability to
  control them even when your primary internet connection is offline.
* Support for per-device modes and scenes.
* Support for the undocumented AWS IoT interface to your devices, provides
  low latency status updates.
* Support for the new [Platform
  API](https://developer.govee.com/reference/get-you-devices) in case the AWS
  IoT or LAN control is unavailable.

|Feature|Requires|Notes|
|-------|--------|-------------|
|DIY Scenes|API Key|Find in the list of Effects for the light in Home Assistant|
|Music Modes|API Key|Find in the list of Effects for the light in Home Assistant|
|Tap-to-Run / One Click Scene|Account for import, LAN for execution|Saved locally; find under Scenes and the `Govee to MQTT` device. See limitations below.|
|Live Device Status Updates|LAN and/or IoT|Devices typically report most changes within a couple of seconds.|
|Segment Color|API Key|Find the `Segment 00X` light entities associated with your main light device in Home Assistant|

* `API Key` means that you have [applied for a key from Govee](https://developer.govee.com/reference/apply-you-govee-api-key)
  and have configured it for use in govee2mqtt
* `IoT` means that you have configured your Govee account email and password for
  use in govee2mqtt, which will then attempt to use the
  *undocumented and likely unsupported* AWS MQTT-based IoT service
* `LAN` means that you have enabled the [Govee LAN API](https://app-h5.govee.com/user-manual/wlan-guide)
  on supported devices and that the LAN API protocol is functional on your network

## Usage

### Offline Tap-to-Run scenes (this fork)

The bridge imports Tap-to-Run commands at startup and every ten minutes when
Govee account access is available. Each successful import adds new scenes and
replaces commands of existing scenes with the same group/name. Import never
executes a scene. Failed imports retain the last saved copy; scenes missing from
a response are retained too.

Tap-to-Run import uses the Govee app account session, not a Community token.
Valid sessions are reused; expired sessions renew with the configured credentials
and stable client ID. An HTTP 401 or HTTP 200 with JSON status 401 discards only
that account session and retries the scene read once. Other cloud errors keep
saved scenes without repeated login attempts. A genuine Govee 2FA challenge
still requires verification; normal refresh does not request a code preemptively.

Scenes and device naming metadata are stored in `govee2mqtt-offline.sqlite` in
`GOVEE_CACHE_DIR` (the add-on's persistent data directory). This database does not
expire and is not removed by **Purge Caches**. Import once with internet access
before relying on offline operation. Cloud discovery failures no longer prevent
startup; discovery and import are retried periodically.

HA and HTTP One-Click calls use saved commands over LAN, without cloud fallback.
This initial implementation supports `ptReal` scene packets, whole-device
`turn`, `colorwc`, and percentage `brightness` commands (0-100), without a
model allowlist. Brightness values are forwarded unchanged to the LAN command.
Other command formats, execution rules, and missing LAN targets
are rejected before sending any scene command, with the reason in the log and
the HTTP scene list. Device/firmware support for `ptReal` must be tested: a
successful UDP send is not confirmation that the effect was applied. The
explicit `undoc one-click` diagnostic CLI command still uses AWS.

* [Installing the HASS Add-On](docs/ADDON.md) - for HAOS and Supervised HASS users
* [Running it in Docker](docs/DOCKER.md)
* [Configuration](docs/CONFIG.md)

## Have a question?

* [Is my device supported?](docs/SKUS.md)
* [Check out the FAQ](docs/FAQ.md)

## Want to show your support or gratitude?

It takes significant effort to build, maintain and support users of software
like this. If you can spare something to say thanks, it is appreciated!

* [Sponsor me on Github](https://github.com/sponsors/wez)
* [Sponsor me on Patreon](https://patreon.com/WezFurlong)
* [Sponsor me on Ko-Fi](https://ko-fi.com/wezfurlong)
* [Sponsor me via liberapay](https://liberapay.com/wez)

## Credits

This work is based on my earlier work with [Govee LAN
Control](https://github.com/wez/govee-lan-hass/).

The AWS IoT support was made possible by the work of @bwp91 in
[homebridge-govee](https://github.com/bwp91/homebridge-govee/).
