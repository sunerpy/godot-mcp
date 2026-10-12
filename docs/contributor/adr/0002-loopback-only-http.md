# 0002 — HTTP binds loopback only, and TLS lives in a proxy

**Status:** accepted.

## Decision

The HTTP transport is off by default. When enabled, the backend binds a loopback address
in every mode, and a non-loopback bind is rejected by configuration validation. This
process terminates no TLS and has no certificate or key setting.

External reachability is achieved by putting a same-host reverse proxy in front. The
proxy terminates TLS, authenticates the end user, and forwards to the loopback backend
with its own proxy-hop credential.

## Why

**A listener is the one thing you cannot take back.** Once a process can bind a public
address, every subsequent decision about it is a mitigation. Making the bind
unconditionally loopback removes the whole class of "accidentally exposed" from the
configuration surface — there is no key to get wrong.

**TLS in-process is a large, ongoing surface for no gain.** Certificates expire, need
rotating, and need a public name that is somebody's operational responsibility.
Reverse proxies already do this well, and every deployment that needs external access
already has one. Adding a certificate setting here would let an operator get TLS wrong in
a place they were not expecting to have to get it right.

**Two independent facts are stronger than one.** With a proxy in front, the backend can
require both that the request came through *your* proxy and that the end user is the one
configured principal. Neither alone would be enough: a proxy-hop credential says nothing
about which person is calling, and a bearer token says nothing about whether it arrived
through the path you control.

## The rule this exists to prevent

Reaching the loopback socket is not an authorization. Any local process, running as any
user, can connect to a loopback port. That is why every HTTP mode authenticates, and why
there is no mode in which arriving on the socket is sufficient. A design that treated
loopback as identity would be wrong on any multi-user machine and on any machine running
untrusted local code.

## Consequences

* An operator who needs off-host access must run a proxy. That is more setup than a
  configuration flag, and it is the point: the setup is where the security decisions
  belong, and they are visible there.
* This process cannot detect a misconfigured proxy. That risk is stated explicitly in the
  [threat model](../../security/threat-model.md) rather than being papered over.
* Single-client deployments do not need HTTP at all, and stdio remains the default.

## Alternatives rejected

**A configurable bind address with a warning.** A warning is not a boundary. The
configurations people actually ship are the ones that started as a quick local
experiment, and a warning does not survive that.

**An opt-in TLS listener.** This would duplicate the proxy's job with a smaller
implementation and a worse operational story, and would create a second place where the
public name and certificate can be wrong.
