# Data and network

This page lists everything godot-mcp keeps and every place it sends data. It runs on your machine,
serves the one client that started it, and reports to no one.

## What it keeps

Everything lives in the project's own `.godot-mcp/` directory: the lock that keeps one server per
project, the durable journal of its transactions, the analysis index, the audit log, the
rendezvous file the editor add-on reads, the records and captured output of its sessions and
exports, and the artifacts they produced — captured images and recordings, and exported builds —
until their retention ends. Outside the project it writes only what you configure, such as a log file.
Delete `.godot-mcp/` while no server runs to remove all of it.

## What it sends, and where

| Destination | When | What goes there |
| --- | --- | --- |
| The MCP client that started it, over stdio | Every request | The answer: the project data the request asked for |
| A loopback HTTP client | Only with HTTP configured, and only with the secret or the proxy hop's credential | The same answers as over stdio |
| Your identity provider | Only with `proxied_oidc`, to fetch its signing keys | A request for the published keys, from the issuer you configured |
| The Godot processes it starts | Every engine run, game and editor session | The project, as Godot reads it; the probe and the bridge talk over loopback |
| The editor add-on, over loopback | Only with the add-on connected | Editor requests and their answers |

There is no telemetry and no update check, and the server never downloads an engine or export
templates.

## The log

Diagnostics go to stderr, or to a rolling file you configure, never to stdout, which carries only
MCP frames. Secrets, tokens and source text are redacted before a line is written, and an answer
names files by their project-relative path rather than by where they sit on your disk.
