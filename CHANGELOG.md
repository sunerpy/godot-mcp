# Changelog

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
