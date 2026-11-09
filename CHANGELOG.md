# Changelog

## [0.8.0](https://github.com/sunerpy/godot-mcp/compare/v0.7.0...v0.8.0) (2026-11-09)


### Features

* **recipes:** add AnimationTree, 3D particle, navigation, sound and material recipes ([#68](https://github.com/sunerpy/godot-mcp/issues/68)) ([ed14fb3](https://github.com/sunerpy/godot-mcp/commit/ed14fb3fc3727d2332a0d31b834b69d0e701ce3d))

## [0.7.0](https://github.com/sunerpy/godot-mcp/compare/v0.6.0...v0.7.0) (2026-11-08)


### Features

* add an export skill and correct the shipped skills ([#62](https://github.com/sunerpy/godot-mcp/issues/62)) ([5842ba4](https://github.com/sunerpy/godot-mcp/commit/5842ba4552da2c31dca03ae5adcf60621e1a09e4))
* step a running game by physics frames ([#60](https://github.com/sunerpy/godot-mcp/issues/60)) ([e9d7e73](https://github.com/sunerpy/godot-mcp/commit/e9d7e735ede0da21465020a9dce2269f9c0ab925))


### Bug Fixes

* let a hung editor's session go even while its status waits ([#58](https://github.com/sunerpy/godot-mcp/issues/58)) ([352f2e7](https://github.com/sunerpy/godot-mcp/commit/352f2e796b2e6c82d9d989352cea72c16bcb302e))
* make the remaining argument checks agree with their schemas ([#59](https://github.com/sunerpy/godot-mcp/issues/59)) ([1002da8](https://github.com/sunerpy/godot-mcp/commit/1002da87f9f53b5bfebca843b9897c1da03f1b33))


### Performance Improvements

* **server:** define once each field schema a value-taking tool repeats ([#63](https://github.com/sunerpy/godot-mcp/issues/63)) ([6964d9a](https://github.com/sunerpy/godot-mcp/commit/6964d9a4a30d06fa05e8cc18669693abdce34907))

## [0.6.0](https://github.com/sunerpy/godot-mcp/compare/v0.5.0...v0.6.0) (2026-11-08)


### Features

* **cli:** add godot-mcp skill install ([#50](https://github.com/sunerpy/godot-mcp/issues/50)) ([91ca790](https://github.com/sunerpy/godot-mcp/commit/91ca790e8bfb4153f8e6a722a9ca943ecfae48c8))
* commands a project declares, listed and called through the editor ([#56](https://github.com/sunerpy/godot-mcp/issues/56)) ([c576137](https://github.com/sunerpy/godot-mcp/commit/c576137c60a6f69bf66887a8c1b24cca1d54751f))
* **engine:** pin Godot 4.7.2 in the release lock ([#48](https://github.com/sunerpy/godot-mcp/issues/48)) ([26334a1](https://github.com/sunerpy/godot-mcp/commit/26334a13c0713b76385f2e9208b3893bc8378cb1))
* **server:** generate initialize instructions from the profile ([#51](https://github.com/sunerpy/godot-mcp/issues/51)) ([cf23e79](https://github.com/sunerpy/godot-mcp/commit/cf23e79f2403d02eea331c76a56885ac783b03b9))


### Bug Fixes

* **editor:** settle unanswered editor changes instead of blocking writes for good ([#53](https://github.com/sunerpy/godot-mcp/issues/53)) ([d658ac3](https://github.com/sunerpy/godot-mcp/commit/d658ac3c64200dd815af32d9fadb3e256b8da8f3))
* **schema:** align name-like fields with their handler checks ([#49](https://github.com/sunerpy/godot-mcp/issues/49)) ([668646f](https://github.com/sunerpy/godot-mcp/commit/668646f6cf89ad20c8cea6f0d2fd14f2de119817))


### Performance Improvements

* **server:** give each tool schema only the definitions it uses ([#52](https://github.com/sunerpy/godot-mcp/issues/52)) ([68607d4](https://github.com/sunerpy/godot-mcp/commit/68607d4cc0b0b7b2826242ee5bf8677d054db93b))

## [0.5.0](https://github.com/sunerpy/godot-mcp/compare/v0.4.0...v0.5.0) (2026-11-06)


### Features

* **http:** add the open mode, which serves a development network with no credential or one token ([1487e6f](https://github.com/sunerpy/godot-mcp/commit/1487e6ff0f1fa0beca19286cf0682a4bd261777b))
* **recipes:** add scene.recipe.apply, which applies one of ten scene recipes by name as one journaled batch or one undoable editor action ([1487e6f](https://github.com/sunerpy/godot-mcp/commit/1487e6ff0f1fa0beca19286cf0682a4bd261777b))
* **recipes:** add the smoke, sparks, rain, snow and explosion particle presets ([1487e6f](https://github.com/sunerpy/godot-mcp/commit/1487e6ff0f1fa0beca19286cf0682a4bd261777b))
* **recipes:** offer every recipe as an MCP prompt, godot-mcp.recipe.&lt;name&gt;, in the editor and full profiles ([1487e6f](https://github.com/sunerpy/godot-mcp/commit/1487e6ff0f1fa0beca19286cf0682a4bd261777b))

## [0.4.0](https://github.com/sunerpy/godot-mcp/compare/v0.3.0...v0.4.0) (2026-11-06)


### Features

* **http:** close an idle HTTP session after http.session_idle_timeout_ms, one hour by default ([12b3b0e](https://github.com/sunerpy/godot-mcp/commit/12b3b0e01a5a2521fd65b42c30b7f183e98e959c))
* **http:** set how long an HTTP session is honoured with http.session_lifetime_ms ([12b3b0e](https://github.com/sunerpy/godot-mcp/commit/12b3b0e01a5a2521fd65b42c30b7f183e98e959c))


### Bug Fixes

* **http:** answer -32002 when resources/read refuses an artifact ([12b3b0e](https://github.com/sunerpy/godot-mcp/commit/12b3b0e01a5a2521fd65b42c30b7f183e98e959c))
* **http:** bind artifacts to the principal, so every session with the same credential reads them ([12b3b0e](https://github.com/sunerpy/godot-mcp/commit/12b3b0e01a5a2521fd65b42c30b7f183e98e959c))

## [0.3.0](https://github.com/sunerpy/godot-mcp/compare/v0.2.0...v0.3.0) (2026-11-05)


### Features

* **analysis:** audit dependencies, measure, search and list signals, UIDs, exported properties and nodes across a project ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **editor:** read the edited scene, move the editor camera, answer dialogs, close script tabs, restart, and read performance and resource previews ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **editor:** show the server's status in an editor panel ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **editor:** write property paths, create any resource class and call more editing methods ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **engine:** read engine values from scenes and resources, check shaders, bake navigation and save audio bus layouts ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **export:** export patch packs and Android APKs, and deploy an APK to a device ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **runtime:** run test scenarios with time scale and image comparison ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **runtime:** set properties, find and move nodes, wait, sample, record and replay input, press UI and query navigation in a managed game ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **server:** apply batches to several scenes and resources in one transaction ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **server:** run caller-written GDScript headlessly, in the live editor and in a managed game when the operator allows scripts ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))


### Bug Fixes

* **editor:** wait for the editor's own scan or import before scanning ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))
* **engine:** refuse a write whose value Godot replaces instead of answering success ([7d69450](https://github.com/sunerpy/godot-mcp/commit/7d69450c870f17b7582945aa35055a0856b1beae))

## [0.2.0](https://github.com/sunerpy/godot-mcp/compare/v0.1.2...v0.2.0) (2026-11-03)


### Features

* **editor:** inspect, edit, save, capture, scan and run through the live editor ([#36](https://github.com/sunerpy/godot-mcp/issues/36)) ([0d729fd](https://github.com/sunerpy/godot-mcp/commit/0d729fdc697d721205bb12177f9bfbb98e0247e3))
* **editor:** typed editing calls for tilemaps, animations, curves and themes ([#37](https://github.com/sunerpy/godot-mcp/issues/37)) ([22e1a9f](https://github.com/sunerpy/godot-mcp/commit/22e1a9f8fa5d306d9ee1523d32deffa39b374230))


### Bug Fixes

* **deps:** update rustls, rustls-webpki, webpki-root-certs and tokio for RustSec advisories ([#38](https://github.com/sunerpy/godot-mcp/issues/38)) ([25b138a](https://github.com/sunerpy/godot-mcp/commit/25b138ab45ed97c40fa8e90b49e8518071dde902))
* **server:** keep project settings and import caches coherent while an editor is connected ([#35](https://github.com/sunerpy/godot-mcp/issues/35)) ([aeae3e9](https://github.com/sunerpy/godot-mcp/commit/aeae3e9f0eefaeec04da127d582e1137023838ba))

## [0.1.2](https://github.com/sunerpy/godot-mcp/compare/v0.1.1...v0.1.2) (2026-10-13)


### Bug Fixes

* **server:** report the build's version in serverInfo and health ([#26](https://github.com/sunerpy/godot-mcp/issues/26)) ([dece784](https://github.com/sunerpy/godot-mcp/commit/dece7843319f782448cd660840a4a9107c86952b))

## [0.1.1](https://github.com/sunerpy/godot-mcp/compare/v0.1.0...v0.1.1) (2026-10-13)


### Bug Fixes

* **analysis:** report a script whose whole tree failed to parse as unresolved ([#25](https://github.com/sunerpy/godot-mcp/issues/25)) ([d87cac8](https://github.com/sunerpy/godot-mcp/commit/d87cac80e67d7ab6828cbf6ca445d890b6914122))
* **server:** connect clients that probe with server/discover ([#20](https://github.com/sunerpy/godot-mcp/issues/20)) ([9776201](https://github.com/sunerpy/godot-mcp/commit/977620150b71f0aaf46fd1152776723f99690f63))

## 0.1.0 (2026-10-12)


### Bug Fixes

* **domain:** retry a contended lease lock briefly before refusing it ([#8](https://github.com/sunerpy/godot-mcp/issues/8)) ([4c019e1](https://github.com/sunerpy/godot-mcp/commit/4c019e165a91f28a57ca463ec16129232bd1a60a))
* **engine:** read the supervisor's report after it closes the control channel on macOS ([#4](https://github.com/sunerpy/godot-mcp/issues/4)) ([ea10ba0](https://github.com/sunerpy/godot-mcp/commit/ea10ba00f45969bcceb2d5f94cbd72e3c683e7e6))
